//! One-shot: the blobs from before captures were WebP, re-encoded with their provenance: a PNG's
//! from its `tEXt` chunks, an AVIF's Exif as is. Remove once prod has converted.

use std::path::{Path, PathBuf};

use eyre::WrapErr;
use futures::{StreamExt, TryStreamExt};

use super::{Store, blobs::BlobStore};
use crate::webp;

impl Store {
	/// Every `.png`/`.avif` blob a capture names becomes WebP, the rows and pending payloads
	/// naming it follow, then the old file goes; one no capture names is just removed.
	/// Resumable per blob.
	pub(crate) async fn convert_legacy_blobs(&self, blobs: &BlobStore) -> eyre::Result<()> {
		let olds = legacy_under(blobs.root())?;
		if olds.is_empty() {
			return Ok(());
		}
		tracing::info!(count = olds.len(), "converting captures to WebP");
		let mut done = 0;
		// one at a time: an encode's buffers are the pod's memory budget, and nobody waits on this
		futures::stream::iter(&olds)
			.then(|path| async move { self.convert(blobs, path).await.wrap_err_with(|| format!("converting {}", path.display())) })
			.try_for_each(|()| {
				done += 1;
				if done % 500 == 0 {
					tracing::info!(done, of = olds.len(), "converting captures to WebP");
				}
				std::future::ready(Ok(()))
			})
			.await?;
		tracing::info!(count = olds.len(), "captures converted to WebP");
		Ok(())
	}

	async fn convert(&self, blobs: &BlobStore, path: &Path) -> eyre::Result<()> {
		let old = path.file_stem().and_then(|s| s.to_str()).expect("listed by its extension").to_owned();
		let ext = path.extension().and_then(|e| e.to_str()).expect("listed by its extension");
		let named: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM captures WHERE sha256 = ?").bind(&old).fetch_one(&self.pool).await?;
		if named > 0 {
			let bytes = match tokio::fs::read(path).await {
				Ok(b) => b,
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // another process converted it meanwhile
				Err(e) => return Err(e.into()),
			};
			let new = blobs.put(&reencode(bytes, ext).await?.bytes).await?;
			let mut tx = self.write().await?;
			sqlx::query("UPDATE captures SET sha256 = ?2 WHERE sha256 = ?1").bind(&old).bind(&new).execute(&mut *tx).await?;
			sqlx::query(
				"UPDATE webhook_deliveries SET payload = REPLACE(REPLACE(payload, ?1, ?2), '/captures/' || ?2 || '.' || ?3, '/captures/' || ?2 || '.webp')
				 WHERE payload LIKE '%' || ?1 || '%'",
			)
			.bind(&old)
			.bind(&new)
			.bind(ext)
			.execute(&mut *tx)
			.await?;
			tx.commit().await?;
		}
		match tokio::fs::remove_file(path).await {
			Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()), // NotFound: another process removed it
			_ => Ok(()),
		}
	}
}

async fn reencode(bytes: Vec<u8>, ext: &str) -> eyre::Result<webp::Webp> {
	match ext {
		"png" => reencode_png(bytes).await,
		"avif" => {
			let exif = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(&bytes))?.buf().to_vec();
			webp::encode(bytes, image::ImageFormat::Avif, exif).await
		}
		_ => unreachable!("listed by these extensions"),
	}
}

async fn reencode_png(png: Vec<u8>) -> eyre::Result<webp::Webp> {
	let text: std::collections::HashMap<String, String> = png::Decoder::new(std::io::Cursor::new(&png))
		.read_info()?
		.info()
		.uncompressed_latin1_text
		.iter()
		.map(|t| (t.keyword.clone(), t.text.clone()))
		.collect();
	let get = |k: &str| text.get(k).map(String::as_str).ok_or_else(|| eyre::eyre!("no {k:?} tEXt chunk"));
	let taken = get("Creation Time")?.parse().wrap_err("Creation Time")?;
	let exif = webp::exif(taken, get("Source")?, get("Title")?, get("Review ID")?, get("Software")?)?;
	webp::encode(png, image::ImageFormat::Png, exif).await
}

fn legacy_under(root: &Path) -> eyre::Result<Vec<PathBuf>> {
	let shards = match std::fs::read_dir(root) {
		Ok(d) => d,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()), // nothing stored yet
		Err(e) => return Err(eyre::Report::new(e).wrap_err(format!("listing {}", root.display()))),
	};
	let mut out = Vec::new();
	for shard in shards {
		let shard = shard?.path();
		if !shard.is_dir() {
			continue;
		}
		for f in std::fs::read_dir(&shard)? {
			let f = f?.path();
			if f.extension().is_some_and(|e| e == "png" || e == "avif") {
				out.push(f);
			}
		}
	}
	Ok(out)
}
