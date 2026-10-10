//! What the archive keeps of a screenshot: lossy WebP, its provenance in Exif, so a capture that
//! leaves the archive still says when and where it was taken.

use exif::{Context, Field, In, Tag, Value, experimental::Writer};
use jiff::Timestamp;
use review_archive_core::Capture;

use crate::SCANNER_VERSION;

/// TIFF's, which kamadak-exif does not name.
const DOCUMENT_NAME: Tag = Tag(Context::Tiff, 0x010d);

const QUALITY: f32 = 80.;

/// An encoded capture.
#[derive(Clone, Debug, PartialEq)]
pub struct Webp {
	/// The file.
	pub bytes: Vec<u8>,
	/// In pixels.
	pub width: u32,
	/// In pixels.
	pub height: u32,
}

/// The capture's PNG as WebP, its provenance written in: when, where, of what, by what.
pub async fn provenance(c: &Capture, title: &str, source_review_id: &str) -> eyre::Result<Webp> {
	let exif = exif(c.captured_at, &c.page_url, title, source_review_id, &format!("review_archive {SCANNER_VERSION}"))?;
	encode(c.png.clone(), image::ImageFormat::Png, exif).await
}

/// For Telegram's `sendPhoto`, documented for JPEG/PNG.
pub fn to_png(webp: &[u8]) -> eyre::Result<Vec<u8>> {
	let img = image::load_from_memory_with_format(webp, image::ImageFormat::WebP)?;
	let mut out = std::io::Cursor::new(Vec::new());
	img.write_to(&mut out, image::ImageFormat::Png)?;
	Ok(out.into_inner())
}

/// Text goes into the ASCII-typed fields as raw UTF-8: Exif has no other text type for them,
/// and readers show UTF-8 as written.
pub(crate) fn exif(captured_at: Timestamp, page_url: &str, title: &str, review_id: &str, software: &str) -> eyre::Result<Vec<u8>> {
	let ascii = |tag, s: &str| Field {
		tag,
		ifd_num: In::PRIMARY,
		value: Value::Ascii(vec![s.as_bytes().to_vec()]),
	};
	let fields = [
		ascii(Tag::DateTimeOriginal, &captured_at.strftime("%Y:%m:%d %H:%M:%S").to_string()),
		ascii(Tag::OffsetTimeOriginal, "+00:00"),
		ascii(DOCUMENT_NAME, page_url), // Exif has no source-URL tag
		ascii(Tag::ImageDescription, title),
		ascii(Tag::ImageUniqueID, review_id),
		ascii(Tag::Software, software),
	];
	let mut w = Writer::new();
	for f in &fields {
		w.push_field(f);
	}
	let mut out = std::io::Cursor::new(Vec::new());
	w.write(&mut out, false)?;
	Ok(out.into_inner())
}

/// `exif` is a bare TIFF block, as [`exif`] writes it.
pub(crate) async fn encode(image: Vec<u8>, format: image::ImageFormat, exif: Vec<u8>) -> eyre::Result<Webp> {
	let lease = v_utils::memory_lease::Lease::acquire().await?;
	let webp = tokio::task::spawn_blocking(move || {
		let rgb = image::load_from_memory_with_format(&image, format)?.into_rgb8();
		let (width, height) = rgb.dimensions();
		let encoded = libwebp::Encoder::from_rgb(&rgb, width, height)
			.encode_simple(false, QUALITY)
			.map_err(|e| eyre::eyre!("encoding WebP: {e:?}"))?;
		Ok(Webp {
			bytes: with_exif(&encoded, width, height, &exif),
			width,
			height,
		})
	})
	.await?;
	drop(lease);
	webp
}

/// libwebp's simple file (`RIFF … WEBP VP8 …`) extended with an `EXIF` chunk, which the format
/// only allows after a `VP8X` header that flags it.
fn with_exif(simple: &[u8], width: u32, height: u32, exif: &[u8]) -> Vec<u8> {
	assert!(simple.starts_with(b"RIFF") && &simple[8..16] == b"WEBPVP8 ", "libwebp writes lossy RGB as a simple file");
	let chunk = |out: &mut Vec<u8>, fourcc: &[u8; 4], data: &[u8]| {
		out.extend_from_slice(fourcc);
		out.extend_from_slice(&u32::try_from(data.len()).expect("a capture is far below 4 GiB").to_le_bytes());
		out.extend_from_slice(data);
		if data.len() % 2 == 1 {
			out.push(0);
		}
	};
	let mut vp8x = vec![0x08, 0, 0, 0]; // the Exif flag
	vp8x.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
	vp8x.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
	let mut body = b"WEBP".to_vec();
	chunk(&mut body, b"VP8X", &vp8x);
	body.extend_from_slice(&simple[12..]);
	chunk(&mut body, b"EXIF", exif);
	let mut out = b"RIFF".to_vec();
	out.extend_from_slice(&u32::try_from(body.len()).expect("a capture is far below 4 GiB").to_le_bytes());
	out.extend_from_slice(&body);
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	fn png(w: u32, h: u32) -> Vec<u8> {
		let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 40) as u8, (y * 60) as u8, 200]));
		let mut out = std::io::Cursor::new(Vec::new());
		img.write_to(&mut out, image::ImageFormat::Png).unwrap();
		out.into_inner()
	}

	#[tokio::test]
	async fn exif_survives_in_the_webp() {
		let c = Capture {
			png: png(5, 3),
			captured_at: "2026-09-26T12:00:00Z".parse().unwrap(),
			page_url: "https://maps.google.com/?cid=1".into(),
		};
		let webp = provenance(&c, "Café ☕", "r1").await.unwrap();
		assert_eq!((webp.width, webp.height), (5, 3));
		let exif = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(&webp.bytes)).unwrap();
		let text = |tag| match &exif.get_field(tag, In::PRIMARY).unwrap_or_else(|| panic!("{tag} written")).value {
			Value::Ascii(v) => String::from_utf8(v[0].clone()).unwrap(),
			v => panic!("{v:?}"),
		};
		assert_eq!(text(Tag::DateTimeOriginal), "2026:09:26 12:00:00");
		assert_eq!(text(Tag::OffsetTimeOriginal), "+00:00");
		assert_eq!(text(DOCUMENT_NAME), "https://maps.google.com/?cid=1");
		assert_eq!(text(Tag::ImageDescription), "Café ☕");
		assert_eq!(text(Tag::ImageUniqueID), "r1");
		assert!(text(Tag::Software).starts_with("review_archive "));

		let back = image::load_from_memory_with_format(&to_png(&webp.bytes).unwrap(), image::ImageFormat::Png).unwrap();
		assert_eq!((back.width(), back.height()), (5, 3));
	}

	#[tokio::test]
	async fn rejects_non_png() {
		assert!(encode(b"GIF89a".to_vec(), image::ImageFormat::Png, Vec::new()).await.is_err());
	}
}
