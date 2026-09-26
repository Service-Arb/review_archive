//! One browser profile, one process. Chromium guards its profile with `Singleton*` files
//! that name the host and pid holding it; after a pod restart the host is a new one, and
//! Chromium refuses a profile "in use on another computer" forever. Our own lock says for
//! sure whether anyone still holds the profile, so what Chromium left behind can go.

use std::{
	fs::{File, OpenOptions, TryLockError},
	path::Path,
};

use eyre::WrapErr;

const LOCK_FILE: &str = "review_archive.lock";
/// What Chromium leaves in a profile while it runs, and behind when it dies.
const CHROMIUM_SINGLETONS: &[&str] = &["SingletonLock", "SingletonSocket", "SingletonCookie"];

/// Held for as long as a browser runs on the profile. Released on drop, and by the OS
/// when the process dies, so a crash never leaves it stale.
#[derive(Debug)]
pub struct ProfileLock {
	_file: File,
}

impl ProfileLock {
	pub fn acquire(profile_dir: &Path) -> eyre::Result<Self> {
		std::fs::create_dir_all(profile_dir).wrap_err_with(|| format!("creating {}", profile_dir.display()))?;
		let path = profile_dir.join(LOCK_FILE);
		let file = OpenOptions::new()
			.create(true)
			.write(true)
			.truncate(false)
			.open(&path)
			.wrap_err_with(|| format!("opening {}", path.display()))?;
		match file.try_lock() {
			Ok(()) => {}
			Err(TryLockError::WouldBlock) => eyre::bail!(
				"the browser profile {} is in use by another review_archive process (a running `serve`?); scan through it, or stop it first",
				profile_dir.display()
			),
			Err(TryLockError::Error(e)) => return Err(eyre::Report::new(e).wrap_err(format!("locking {}", path.display()))),
		}
		for name in CHROMIUM_SINGLETONS {
			let p = profile_dir.join(name);
			match std::fs::remove_file(&p) {
				Ok(()) => tracing::info!(path = %p.display(), "removed a stale Chromium profile lock"),
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
				Err(e) => return Err(eyre::Report::new(e).wrap_err(format!("removing {}", p.display()))),
			}
		}
		Ok(Self { _file: file })
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clears_what_a_dead_chromium_left_and_excludes_a_second_holder() {
		let dir = tempfile::tempdir().unwrap();
		// what Chromium leaves: a dangling symlink naming a host that no longer exists
		std::os::unix::fs::symlink("old-pod-hostname-4242", dir.path().join("SingletonLock")).unwrap();
		std::fs::write(dir.path().join("SingletonCookie"), b"x").unwrap();

		let held = ProfileLock::acquire(dir.path()).unwrap();
		assert!(dir.path().join("SingletonLock").symlink_metadata().is_err());
		assert!(!dir.path().join("SingletonCookie").exists());

		let err = ProfileLock::acquire(dir.path()).unwrap_err();
		assert!(format!("{err:#}").contains("in use by another review_archive process"), "{err:#}");

		drop(held);
		ProfileLock::acquire(dir.path()).unwrap();
	}
}
