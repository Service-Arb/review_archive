//! One browser profile, one review_archive process, claimed before Chromium starts: the
//! framework's own lock is only taken at launch, and a run must not be recorded on a profile
//! another process holds.

use std::{
	fs::{File, OpenOptions, TryLockError},
	path::Path,
};

use eyre::WrapErr;

const LOCK_FILE: &str = "review_archive.lock";

/// The OS releases it when the process dies, so a crash never leaves it stale.
#[derive(Debug)]
pub(crate) struct ProfileLock(File);

impl ProfileLock {
	pub(crate) fn acquire(profile_dir: &Path) -> eyre::Result<Self> {
		std::fs::create_dir_all(profile_dir).wrap_err_with(|| format!("creating {}", profile_dir.display()))?;
		let path = profile_dir.join(LOCK_FILE);
		let file = OpenOptions::new()
			.create(true)
			.write(true)
			.truncate(false)
			.open(&path)
			.wrap_err_with(|| format!("opening {}", path.display()))?;
		match file.try_lock() {
			Ok(()) => Ok(Self(file)),
			Err(TryLockError::WouldBlock) => eyre::bail!(
				"the browser profile {} is in use by another review_archive process (a running `serve`?); scan through it, or stop it first",
				profile_dir.display()
			),
			Err(TryLockError::Error(e)) => Err(eyre::Report::new(e).wrap_err(format!("locking {}", path.display()))),
		}
	}
}

impl Drop for ProfileLock {
	fn drop(&mut self) {
		self.0.unlock().expect("we hold the flock");
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn excludes_a_second_holder() {
		let dir = tempfile::tempdir().unwrap();
		let held = ProfileLock::acquire(dir.path()).unwrap();
		let err = ProfileLock::acquire(dir.path()).unwrap_err();
		assert!(format!("{err:#}").contains("in use by another review_archive process"), "{err:#}");
		drop(held);
		ProfileLock::acquire(dir.path()).unwrap();
	}
}
