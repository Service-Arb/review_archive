//! Content-addressed captures: `<root>/<sha256[0..2]>/<sha256>.avif`.

use std::{
	io::Write,
	path::{Path, PathBuf},
};

use eyre::WrapErr;
use review_archive_core::hex;
use sha2::{Digest, Sha256};

/// Captures on disk, named by their SHA-256.
#[derive(Clone, Debug)]
pub struct BlobStore {
	root: PathBuf,
}

impl BlobStore {
	/// A store rooted at `root` (created on first write).
	pub fn new(root: impl Into<PathBuf>) -> Self {
		Self { root: root.into() }
	}

	/// Writes the bytes under their hash and returns it. Writing the same bytes twice is a
	/// no-op; a file already there that does not hash to its name is replaced.
	pub async fn put(&self, bytes: &[u8]) -> eyre::Result<String> {
		let sha = hex(&Sha256::digest(bytes));
		let path = self.path_of(&sha).expect("a sha256 we just computed is a valid blob name");
		match tokio::fs::read(&path).await {
			Ok(existing) if hex(&Sha256::digest(&existing)) == sha => return Ok(sha),
			Ok(_) => tracing::warn!(path = %path.display(), "blob does not hash to its name; rewriting it"),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
			Err(e) => return Err(eyre::Report::new(e).wrap_err(format!("reading {}", path.display()))),
		}
		let bytes = bytes.to_vec();
		// write, fsync, rename, fsync the dir: a crash leaves either no file or the whole one
		tokio::task::spawn_blocking(move || write_durably(&path, &bytes)).await??;
		Ok(sha)
	}

	/// `None` unless `sha` is 64 lowercase hex digits — it arrives from URLs.
	pub fn path_of(&self, sha: &str) -> Option<PathBuf> {
		let valid = sha.len() == 64 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
		valid.then(|| self.root.join(&sha[..2]).join(format!("{sha}.avif")))
	}

	/// Where it lives.
	pub fn root(&self) -> &Path {
		&self.root
	}
}

fn write_durably(path: &Path, bytes: &[u8]) -> eyre::Result<()> {
	let dir = path.parent().expect("blob paths always have a shard dir");
	std::fs::create_dir_all(dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
	// A temp file of its own in the same dir (so the rename is atomic), removed on drop if
	// anything below fails.
	let written = (|| {
		let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
		tmp.write_all(bytes)?;
		tmp.as_file().sync_all()?;
		tmp.persist(path)?;
		std::fs::File::open(dir)?.sync_all()
	})();
	written.wrap_err_with(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn put_is_content_addressed_and_idempotent() {
		let dir = tempfile::tempdir().unwrap();
		let blobs = BlobStore::new(dir.path());
		let a = blobs.put(b"png bytes").await.unwrap();
		assert_eq!(a, blobs.put(b"png bytes").await.unwrap());
		let path = blobs.path_of(&a).unwrap();
		assert!(path.starts_with(dir.path().join(&a[..2])));
		assert_eq!(std::fs::read(path).unwrap(), b"png bytes");
	}

	#[tokio::test]
	async fn a_damaged_blob_is_rewritten() {
		let dir = tempfile::tempdir().unwrap();
		let blobs = BlobStore::new(dir.path());
		let sha = hex(&Sha256::digest(b"png bytes"));
		let path = blobs.path_of(&sha).unwrap();
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		// what a crash between write and rename used to be able to leave behind
		std::fs::write(&path, b"png").unwrap();
		assert_eq!(blobs.put(b"png bytes").await.unwrap(), sha);
		assert_eq!(std::fs::read(&path).unwrap(), b"png bytes");
		let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
		assert_eq!(leftovers.len(), 1, "no temp files left: {leftovers:?}");
	}

	#[test]
	fn rejects_names_that_are_not_hashes() {
		let blobs = BlobStore::new("/x");
		assert!(blobs.path_of("../../etc/passwd").is_none());
		assert!(blobs.path_of(&"A".repeat(64)).is_none());
		assert!(blobs.path_of(&"a".repeat(64)).is_some());
	}
}
