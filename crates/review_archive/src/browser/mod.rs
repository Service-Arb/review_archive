//! The one headless Chromium the `maps` source (and the screenshots of `gbp`) run in.
//!
//! A plain browser at a polite rate: no stealth, no fingerprint masking, no proxies. When
//! Google does not serve the full page, the walk fails and says why.

mod profile;
mod session;

use std::{path::PathBuf, sync::Arc};

use ev_lib::alerts::Artifacts;
use review_archive_core::maps::{WalkEnd, WalkPolicy, Walked, selectors};

use self::{profile::ProfileLock, session::Session};
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
	state: tokio::sync::Mutex<State>,
}

/// The profile, held from [`Browser::claim`] or the first walk until [`Browser::close`].
#[derive(Default)]
struct State {
	/// Claimed, Chromium not started yet.
	lock: Option<ProfileLock>,
	/// Chromium running; it holds the lock.
	session: Option<Session>,
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
				state: tokio::sync::Mutex::default(),
			}),
		}
	}

	/// Takes the profile for this process without starting Chromium: fails, with the
	/// reason, while another process holds it. Held until [`Self::close`].
	pub async fn claim(&self) -> eyre::Result<()> {
		let mut state = self.inner.state.lock().await;
		if state.lock.is_none() && state.session.is_none() {
			state.lock = Some(ProfileLock::acquire(&self.inner.profile_dir)?);
		}
		Ok(())
	}

	/// Opens the place's review list, sorted newest first, and walks it: `policy` says
	/// which cards to screenshot and when to stop; `max` caps the cards read.
	pub async fn walk(&self, place_id: &str, lang: &str, policy: &mut dyn WalkPolicy, max: usize) -> eyre::Result<Walked> {
		let mut state = self.inner.state.lock().await;
		if state.session.is_none() {
			let lock = match state.lock.take() {
				Some(lock) => lock,
				None => ProfileLock::acquire(&self.inner.profile_dir)?,
			};
			state.session = Some(Session::launch(&self.inner.cfg, &self.inner.profile_dir, lock).await?);
		}
		let session = state.session.as_ref().expect("launched just above");
		let walked = match session.open_reviews(place_id, lang).await {
			Ok(Some(opened)) => session.walk(policy, max, opened).await,
			Ok(None) => Ok(Walked::empty(selectors::place_url(place_id, lang))),
			Err(e) => Err(e),
		};
		let walked = match (walked, &self.inner.cfg.artifacts) {
			(Err(e), Some(artifacts)) => Err(eyre::Report::new(e).wrap_err(save_page(session, artifacts).await)),
			(Err(e), None) => Err(e.into()),
			(Ok(mut w), Some(artifacts)) if w.end == WalkEnd::Interrupted => {
				w.warnings.push(save_page(session, artifacts).await);
				Ok(w)
			}
			(Ok(w), _) => Ok(w),
		};
		// A browser that failed under a walk may be dead (a crashed tab, a lost CDP socket):
		// the next walk starts a fresh one rather than fail on it forever.
		if matches!(walked, Err(_) | Ok(Walked { end: WalkEnd::Interrupted, .. }))
			&& let Some(s) = state.session.take()
		{
			s.close().await;
		}
		walked
	}

	/// Stops Chromium, if it runs, and lets go of the profile. The next walk starts a new
	/// one; a service that scans every few hours closes it in between rather than keep it
	/// idle.
	pub async fn close(&self) {
		let mut state = self.inner.state.lock().await;
		state.lock = None;
		if let Some(s) = state.session.take() {
			s.close().await;
		}
	}
}

/// Saves what the page showed when a walk failed; the line that says where, or why it could not.
async fn save_page(session: &Session, artifacts: &Artifacts) -> String {
	match session.save_page(artifacts).await {
		Ok(saved) => saved,
		Err(e) => format!("page not saved: {e:#}"),
	}
}
