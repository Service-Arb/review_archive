//! Content-addressed PNGs: `<root>/<sha256[0..2]>/<sha256>.png`.

use std::path::{Path, PathBuf};

use eyre::WrapErr;
use sha2::{Digest, Sha256};

use crate::domain::hex;

#[derive(Clone, Debug)]
pub struct BlobStore {
	root: PathBuf,
}

impl BlobStore {
	pub fn new(root: impl Into<PathBuf>) -> Self {
		Self { root: root.into() }
	}

	/// Writes the bytes under their hash and returns it. Writing the same bytes twice is a no-op.
	pub async fn put(&self, bytes: &[u8]) -> eyre::Result<String> {
		let sha = hex(&Sha256::digest(bytes));
		let path = self.path_of(&sha).expect("a sha256 we just computed is a valid blob name");
		if tokio::fs::try_exists(&path).await.wrap_err_with(|| format!("checking {}", path.display()))? {
			return Ok(sha);
		}
		let dir = path.parent().expect("blob paths always have a shard dir");
		tokio::fs::create_dir_all(dir).await.wrap_err_with(|| format!("creating {}", dir.display()))?;
		// write-then-rename, so a crash never leaves a truncated file under a valid hash
		let tmp = path.with_extension("png.tmp");
		tokio::fs::write(&tmp, bytes).await.wrap_err_with(|| format!("writing {}", tmp.display()))?;
		tokio::fs::rename(&tmp, &path).await.wrap_err_with(|| format!("renaming into {}", path.display()))?;
		Ok(sha)
	}

	/// `None` unless `sha` is 64 lowercase hex digits — it arrives from URLs.
	pub fn path_of(&self, sha: &str) -> Option<PathBuf> {
		let valid = sha.len() == 64 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
		valid.then(|| self.root.join(&sha[..2]).join(format!("{sha}.png")))
	}

	pub fn root(&self) -> &Path {
		&self.root
	}
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

	#[test]
	fn rejects_names_that_are_not_hashes() {
		let blobs = BlobStore::new("/x");
		assert!(blobs.path_of("../../etc/passwd").is_none());
		assert!(blobs.path_of(&"A".repeat(64)).is_none());
		assert!(blobs.path_of(&"a".repeat(64)).is_some());
	}
}
