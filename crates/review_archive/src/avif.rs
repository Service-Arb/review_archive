//! What the archive keeps of a screenshot: AVIF, its provenance in Exif, so a capture that
//! leaves the archive still says when and where it was taken.

use exif::{Context, Field, In, Tag, Value, experimental::Writer};
use jiff::Timestamp;
use review_archive_core::Capture;

use crate::SCANNER_VERSION;

/// TIFF's, which kamadak-exif does not name.
const DOCUMENT_NAME: Tag = Tag(Context::Tiff, 0x010d);

const QUALITY: f32 = 60.;
const SPEED: u8 = 6;

/// An encoded capture.
#[derive(Clone, Debug, PartialEq)]
pub struct Avif {
	/// The file.
	pub bytes: Vec<u8>,
	/// In pixels.
	pub width: u32,
	/// In pixels.
	pub height: u32,
}

/// The capture's PNG as AVIF, its provenance written in: when, where, of what, by what.
pub async fn provenance(c: &Capture, title: &str, source_review_id: &str) -> eyre::Result<Avif> {
	let exif = exif(c.captured_at, &c.page_url, title, source_review_id, &format!("review_archive {SCANNER_VERSION}"))?;
	encode(c.png.clone(), exif).await
}

/// For what refuses AVIF (Telegram's `sendPhoto`).
pub fn to_png(avif: &[u8]) -> eyre::Result<Vec<u8>> {
	let img = image::load_from_memory_with_format(avif, image::ImageFormat::Avif)?;
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

/// CPU-bound for seconds on a large card, so off the async threads.
pub(crate) async fn encode(png: Vec<u8>, exif: Vec<u8>) -> eyre::Result<Avif> {
	tokio::task::spawn_blocking(move || {
		let rgb = image::load_from_memory_with_format(&png, image::ImageFormat::Png)?.into_rgb8();
		let (width, height) = rgb.dimensions();
		let pixels: Vec<ravif::RGB8> = rgb.pixels().map(|p| ravif::RGB8::new(p[0], p[1], p[2])).collect();
		let encoded = ravif::Encoder::new()
			.with_quality(QUALITY)
			.with_speed(SPEED)
			.with_exif(exif)
			.encode_rgb(ravif::Img::new(&pixels[..], width as usize, height as usize))?;
		Ok(Avif {
			bytes: encoded.avif_file,
			width,
			height,
		})
	})
	.await?
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
	async fn exif_survives_in_the_avif() {
		let c = Capture {
			png: png(5, 3),
			captured_at: "2026-09-26T12:00:00Z".parse().unwrap(),
			page_url: "https://maps.google.com/?cid=1".into(),
		};
		let avif = provenance(&c, "Café ☕", "r1").await.unwrap();
		assert_eq!((avif.width, avif.height), (5, 3));
		let exif = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(&avif.bytes)).unwrap();
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

		let back = image::load_from_memory_with_format(&to_png(&avif.bytes).unwrap(), image::ImageFormat::Png).unwrap();
		assert_eq!((back.width(), back.height()), (5, 3));
	}

	#[tokio::test]
	async fn rejects_non_png() {
		assert!(encode(b"GIF89a".to_vec(), Vec::new()).await.is_err());
	}
}
