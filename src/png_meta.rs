//! Provenance written into the PNG itself, so a screenshot that leaves the archive
//! still says when and where it was taken.

const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Width and height from the IHDR chunk.
pub fn dimensions(png: &[u8]) -> eyre::Result<(u32, u32)> {
	eyre::ensure!(png.len() >= 24 && png.starts_with(SIGNATURE) && &png[12..16] == b"IHDR", "not a PNG");
	let w = u32::from_be_bytes(png[16..20].try_into().expect("4-byte slice"));
	let h = u32::from_be_bytes(png[20..24].try_into().expect("4-byte slice"));
	Ok((w, h))
}

/// Inserts one `tEXt` chunk per entry right after IHDR. Keywords must be 1–79 Latin-1
/// characters; values are written as Latin-1, with anything outside it replaced by `?`
/// (the spec's `iTXt` would carry UTF-8, but `tEXt` is what every viewer shows).
pub fn with_text(png: &[u8], entries: &[(&str, &str)]) -> eyre::Result<Vec<u8>> {
	dimensions(png)?;
	let ihdr_end = 8 + 4 + 4 + 13 + 4;
	eyre::ensure!(png.len() >= ihdr_end, "truncated PNG");
	let mut out = Vec::with_capacity(png.len() + entries.len() * 64);
	out.extend_from_slice(&png[..ihdr_end]);
	for (key, value) in entries {
		eyre::ensure!((1..=79).contains(&key.len()) && key.bytes().all(|b| (32..=126).contains(&b)), "bad tEXt keyword {key:?}");
		let mut data = Vec::with_capacity(key.len() + 1 + value.len());
		data.extend_from_slice(key.as_bytes());
		data.push(0);
		data.extend(value.chars().map(|c| u8::try_from(u32::from(c)).ok().filter(|&b| b != 0).unwrap_or(b'?')));
		let len = u32::try_from(data.len()).map_err(|_| eyre::eyre!("tEXt value too long"))?;
		out.extend_from_slice(&len.to_be_bytes());
		let mut crc = crc32fast::Hasher::new();
		crc.update(b"tEXt");
		crc.update(&data);
		out.extend_from_slice(b"tEXt");
		out.extend_from_slice(&data);
		out.extend_from_slice(&crc.finalize().to_be_bytes());
	}
	out.extend_from_slice(&png[ihdr_end..]);
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tiny_png() -> Vec<u8> {
		let mut out = Vec::new();
		let mut enc = png::Encoder::new(&mut out, 3, 2);
		enc.set_color(png::ColorType::Rgb);
		enc.set_depth(png::BitDepth::Eight);
		enc.write_header().unwrap().write_image_data(&[200; 18]).unwrap();
		out
	}

	#[test]
	fn text_chunks_are_readable_by_a_real_decoder() {
		let tagged = with_text(&tiny_png(), &[("Creation Time", "2026-09-26T12:00:00Z"), ("Comment", "café ☕")]).unwrap();
		assert_eq!(dimensions(&tagged).unwrap(), (3, 2));
		let reader = png::Decoder::new(std::io::Cursor::new(&tagged)).read_info().unwrap();
		let texts: Vec<(String, String)> = reader.info().uncompressed_latin1_text.iter().map(|t| (t.keyword.clone(), t.text.clone())).collect();
		assert_eq!(texts, [("Creation Time".into(), "2026-09-26T12:00:00Z".into()), ("Comment".into(), "café ?".into())]);
	}

	#[test]
	fn rejects_non_png() {
		assert!(with_text(b"GIF89a", &[]).is_err());
	}
}
