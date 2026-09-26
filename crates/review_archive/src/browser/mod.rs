//! The one headless Chromium the `maps` source (and the screenshots of `gbp`) run in.
//!
//! A plain browser at a polite rate: no stealth, no fingerprint masking, no proxies. When
//! Google does not serve the full page, the walk fails and says why.

mod profile;
mod session;

use std::{path::PathBuf, sync::Arc};

use review_archive_core::maps::{WalkPolicy, Walked, selectors};

use self::session::Session;
use crate::config::BrowserConfig;

/// A handle on one Chromium, launched on first use and shared by every clone.
///
/// Walks through one handle run one at a time: the browser has one tab. Callers that
/// already run a scan loop can create one and hand clones to [`Archive::open_with_browser`](crate::Archive::open_with_browser)
/// and to their own code. One profile dir takes one browser at a time, across processes
/// too: a second one fails with the reason.
#[derive(Clone)]
pub struct Browser {
	inner: Arc<Inner>,
}

struct Inner {
	cfg: BrowserConfig,
	profile_dir: PathBuf,
	session: tokio::sync::Mutex<Option<Session>>,
}

impl std::fmt::Debug for Browser {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Browser").field("profile_dir", &self.inner.profile_dir).finish_non_exhaustive()
	}
}

impl Browser {
	/// A browser on this profile dir, which is what remembers the consent answer between
	/// runs. Nothing starts until the first walk.
	pub fn new(cfg: BrowserConfig, profile_dir: PathBuf) -> Self {
		Self {
			inner: Arc::new(Inner {
				cfg,
				profile_dir,
				session: tokio::sync::Mutex::new(None),
			}),
		}
	}

	/// Opens the place's review list, sorted newest first, and walks it: `policy` says
	/// which cards to screenshot and when to stop; `max` caps the cards read.
	pub async fn walk(&self, place_id: &str, lang: &str, policy: &mut dyn WalkPolicy, max: usize) -> eyre::Result<Walked> {
		let mut guard = self.inner.session.lock().await;
		if guard.is_none() {
			*guard = Some(Session::launch(&self.inner.cfg, &self.inner.profile_dir).await?);
		}
		let session = guard.as_ref().expect("launched just above");
		match session.open_reviews(place_id, lang).await? {
			Some(opened) => session.walk(policy, max, opened).await,
			None => Ok(Walked::empty(selectors::place_url(place_id, lang))),
		}
	}

	/// Stops Chromium, if it runs. The next walk starts a new one; a service that scans
	/// every few hours closes it in between rather than keep it idle.
	pub async fn close(&self) {
		if let Some(s) = self.inner.session.lock().await.take() {
			s.close().await;
		}
	}
}
