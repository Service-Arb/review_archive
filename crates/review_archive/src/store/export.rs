//! A target's reviews and their first screenshots, as a directory or a `.zip`.

use std::{fs::File, io::Write, path::PathBuf};

use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{TargetId, dto::ReviewDto, fmt_ts};
use serde::Serialize;

use super::{Store, blobs::BlobStore};

#[derive(Serialize)]
struct Manifest {
	exported_at: String,
	target: TargetInfo,
	since: Option<String>,
	reviews: Vec<Entry>,
}

#[derive(Serialize)]
struct TargetInfo {
	id: i64,
	label: String,
	kind: review_archive_core::TargetKind,
	place_id: String,
}

#[derive(Serialize)]
struct Entry {
	#[serde(flatten)]
	review: ReviewDto,
	/// Path of the capture inside the export, when there is one.
	capture: Option<String>,
}

/// What an export wrote.
#[derive(Debug, Default, PartialEq)]
pub struct Exported {
	/// Reviews in `manifest.json`.
	pub reviews: usize,
	/// Captures beside it.
	pub captures: usize,
}

/// Where an export is written.
#[derive(Debug)]
pub enum Destination {
	/// A directory, or a `.zip` when the path ends so.
	Path(PathBuf),
	/// A zip, into this file (a temp file the caller streams from, say).
	Zip(File),
}

/// A target's reviews (first seen at or after `since`) and their first screenshots:
/// `manifest.json` + `captures/*.avif`.
pub async fn export(store: &Store, blobs: &BlobStore, target: TargetId, since: Option<Timestamp>, out: Destination, now: Timestamp) -> eyre::Result<Exported> {
	let t = store.target(target).await?;
	let rows = store.reviews(target, since, None).await?;
	let mut files: Vec<(String, PathBuf)> = Vec::new();
	let mut entries = Vec::with_capacity(rows.len());
	for review in rows {
		let capture = match review.capture_sha256.as_deref().and_then(|sha| blobs.path_of(sha).map(|p| (sha.to_owned(), p))) {
			Some((sha, path)) => {
				let name = format!("captures/{}_{}.avif", review.id, &sha[..12]);
				files.push((name.clone(), path));
				Some(name)
			}
			None => None,
		};
		entries.push(Entry { review, capture });
	}
	let manifest = Manifest {
		exported_at: fmt_ts(now),
		target: TargetInfo {
			id: t.id.0,
			label: t.label,
			kind: t.kind,
			place_id: t.place_id,
		},
		since: since.map(fmt_ts),
		reviews: entries,
	};
	let json = serde_json::to_vec_pretty(&manifest)?;
	let result = Exported {
		reviews: manifest.reviews.len(),
		captures: files.len(),
	};
	// zip and the copies are synchronous file work
	tokio::task::spawn_blocking(move || write_out(out, &json, &files)).await??;
	Ok(result)
}

fn write_out(out: Destination, manifest: &[u8], files: &[(String, PathBuf)]) -> eyre::Result<()> {
	let out = match out {
		Destination::Zip(file) => return write_zip(file, manifest, files),
		Destination::Path(out) => out,
	};
	if out.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
		if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
			std::fs::create_dir_all(dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
		}
		let file = File::create(&out).wrap_err_with(|| format!("creating {}", out.display()))?;
		return write_zip(file, manifest, files);
	}
	std::fs::create_dir_all(out.join("captures")).wrap_err_with(|| format!("creating {}", out.display()))?;
	std::fs::write(out.join("manifest.json"), manifest)?;
	for (name, src) in files {
		std::fs::copy(src, out.join(name)).wrap_err_with(|| format!("copying {}", src.display()))?;
	}
	Ok(())
}

fn write_zip(file: File, manifest: &[u8], files: &[(String, PathBuf)]) -> eyre::Result<()> {
	let mut zip = zip::ZipWriter::new(file);
	let deflated = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
	// AVIFs are already compressed
	let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
	zip.start_file("manifest.json", deflated)?;
	zip.write_all(manifest)?;
	for (name, src) in files {
		let bytes = std::fs::read(src).wrap_err_with(|| format!("reading {}", src.display()))?;
		zip.start_file(name.as_str(), stored)?;
		zip.write_all(&bytes)?;
	}
	zip.finish()?;
	Ok(())
}
