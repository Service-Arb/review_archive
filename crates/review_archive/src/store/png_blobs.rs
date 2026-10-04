//! One-shot: the PNG blobs from before captures were AVIF, re-encoded with the provenance
//! their `tEXt` chunks carry. Remove once prod has converted.

use std::path::{Path, PathBuf};

use eyre::WrapErr;
use futures::{StreamExt, TryStreamExt};

use super::{Store, blobs::BlobStore};
use crate::avif;

impl Store {
	/// Every `.png` blob a capture names becomes AVIF, the rows and pending payloads naming
	/// it follow, then the PNG goes; one no capture names is just removed. Resumable per blob.
	pub(crate) async fn convert_png_blobs(&self, blobs: &BlobStore) -> eyre::Result<()> {
		let pngs = pngs_under(blobs.root())?;
		if pngs.is_empty() {
			return Ok(());
		}
		tracing::info!(count = pngs.len(), "converting PNG captures to AVIF");
		// one encode keeps ~2 cores busy
		let parallel = std::thread::available_parallelism()?.get();
		let mut done = 0;
		futures::stream::iter(&pngs)
			.map(|path| async move { self.convert(blobs, path).await.wrap_err_with(|| format!("converting {}", path.display())) })
			.buffer_unordered(parallel)
			.try_for_each(|()| {
				done += 1;
				if done % 500 == 0 {
					tracing::info!(done, of = pngs.len(), "converting PNG captures");
				}
				std::future::ready(Ok(()))
			})
			.await?;
		tracing::info!(count = pngs.len(), "PNG captures converted");
		Ok(())
	}

	async fn convert(&self, blobs: &BlobStore, path: &Path) -> eyre::Result<()> {
		let old = path.file_stem().and_then(|s| s.to_str()).expect("listed by its .png extension").to_owned();
		let named: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM captures WHERE sha256 = ?").bind(&old).fetch_one(&self.pool).await?;
		if named > 0 {
			let png = match tokio::fs::read(path).await {
				Ok(b) => b,
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // another process converted it meanwhile
				Err(e) => return Err(e.into()),
			};
			let new = blobs.put(&reencode(png).await?.bytes).await?;
			let mut tx = self.write().await?;
			sqlx::query("UPDATE captures SET sha256 = ?2 WHERE sha256 = ?1").bind(&old).bind(&new).execute(&mut *tx).await?;
			sqlx::query(
				"UPDATE webhook_deliveries SET payload = REPLACE(REPLACE(payload, ?1, ?2), '/captures/' || ?2 || '.png', '/captures/' || ?2 || '.avif')
				 WHERE payload LIKE '%' || ?1 || '%'",
			)
			.bind(&old)
			.bind(&new)
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

async fn reencode(png: Vec<u8>) -> eyre::Result<avif::Avif> {
	let text: std::collections::HashMap<String, String> = png::Decoder::new(std::io::Cursor::new(&png))
		.read_info()?
		.info()
		.uncompressed_latin1_text
		.iter()
		.map(|t| (t.keyword.clone(), t.text.clone()))
		.collect();
	let get = |k: &str| text.get(k).map(String::as_str).ok_or_else(|| eyre::eyre!("no {k:?} tEXt chunk"));
	let taken = get("Creation Time")?.parse().wrap_err("Creation Time")?;
	let exif = avif::exif(taken, get("Source")?, get("Title")?, get("Review ID")?, get("Software")?)?;
	avif::encode(png, exif).await
}

fn pngs_under(root: &Path) -> eyre::Result<Vec<PathBuf>> {
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
			if f.extension().is_some_and(|e| e == "png") {
				out.push(f);
			}
		}
	}
	Ok(out)
}
