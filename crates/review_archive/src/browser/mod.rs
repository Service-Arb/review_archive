//! The one headless Chromium the `maps` source (and the screenshots of `gbp`) run in.
//!
//! A plain browser at a polite rate: no stealth, no fingerprint masking, no proxies. When
//! Google does not serve the full page, the walk fails and says why.

mod profile;
mod session;

use std::{path::PathBuf, sync::Arc};

use review_archive_core::maps::{WalkEnd, WalkPolicy, Walked, selectors};

use self::{profile::ProfileLock, session::Session};
use crate::{SessionError, config::BrowserConfig};

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
	lock: Option<ProfileLock>,
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
		if state.lock.is_none() {
			state.lock = Some(ProfileLock::acquire(&self.inner.profile_dir)?);
		}
		Ok(())
	}

	/// Opens the place's review list, sorted newest first, and walks it: `policy` says
	/// which cards to screenshot and when to stop; `max` caps the cards read.
	pub async fn walk(&self, place_id: &str, lang: &str, policy: &mut dyn WalkPolicy, max: usize) -> eyre::Result<Walked> {
		let mut state = self.inner.state.lock().await;
		if state.lock.is_none() {
			state.lock = Some(ProfileLock::acquire(&self.inner.profile_dir)?);
		}
		if state.session.is_none() {
			state.session = Some(Session::launch(&self.inner.cfg, &self.inner.profile_dir).await?);
		}
		let session = state.session.as_ref().expect("launched just above");
		let (walked, broken) = match session.page().await {
			Ok(mut page) => {
				let walked = match page.open_reviews(place_id, lang).await {
					Ok(Some(opened)) => page.walk(policy, max, opened).await,
					Ok(None) => Ok(Walked::empty(selectors::place_url(place_id, lang))),
					Err(e) => Err(e),
				};
				let walked = match walked {
					Err(e) => {
						let saved = match &e {
							SessionError::Browser(b) => b.capture.as_deref().cloned(),
							_ => page.save("walk-failed").await,
						};
						match saved {
							Some(saved) => Err(eyre::Report::new(e).wrap_err(page_line(saved))),
							None => Err(e.into()),
						}
					}
					Ok(mut w) if w.end == WalkEnd::Interrupted => {
						w.warnings.extend(page.save("walk-interrupted").await.map(page_line));
						Ok(w)
					}
					Ok(w) => Ok(w),
				};
				match (page.close().await, walked) {
					(Ok(()), walked) => (walked, false),
					(Err(e), Ok(mut w)) => {
						w.warnings.push(format!("closing the tab after the walk: {e:#}"));
						(Ok(w), true)
					}
					// the walk's own failure says more than the tab that would not close after it
					(Err(_), Err(e)) => (Err(e), true),
				}
			}
			Err(e) => (Err(e.into()), true),
		};
		// A browser that failed under a walk may be dead (a crashed tab, a lost driver):
		// the next walk starts a fresh one rather than fail on it forever.
		if (broken || matches!(walked, Err(_) | Ok(Walked { end: WalkEnd::Interrupted, .. })))
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
		if let Some(s) = state.session.take() {
			s.close().await;
		}
		state.lock = None;
	}
}

/// Where the page a walk ended on was saved, as `[<path>]`s the alerts attach; or why it was not.
fn page_line(saved: Result<browser_manipulation::Capture, String>) -> String {
	match saved {
		Ok(c) => format!("page [{}] [{}]", c.png.display(), c.html.display()),
		Err(e) => format!("page not saved: {e}"),
	}
}
