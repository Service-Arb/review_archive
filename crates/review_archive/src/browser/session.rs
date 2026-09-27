//! Driving a plain headless Chromium over CDP. No stealth, no fingerprint masking:
//! the scanner is what it looks like, and a block from Google fails the run.

use std::{
	collections::HashSet,
	path::{Path, PathBuf},
	time::Duration,
};

use chromiumoxide::{Browser, BrowserConfig as CdpConfig, Page, cdp::browser_protocol::page::CaptureScreenshotFormat, handler::viewport::Viewport};
use ev_lib::alerts::Artifacts;
use eyre::WrapErr;
use futures::StreamExt;
use jiff::Timestamp;
use review_archive_core::{
	Capture,
	maps::{
		SCREEN, WalkEnd, WalkPolicy, Walked, WalkedCard, parse,
		selectors::{self as sel, js},
	},
};
use serde::Serialize;
use tokio::task::JoinHandle;

use super::profile::ProfileLock;
use crate::{SessionError, config::BrowserConfig};

const STEP_WAIT: Duration = Duration::from_millis(1500);
/// Consecutive scrolls without a new card before the feed counts as ended.
const END_AFTER_IDLE_STEPS: u32 = 5;
const UI_TIMEOUT: Duration = Duration::from_secs(20);
const SORT_TIMEOUT: Duration = Duration::from_secs(10);
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(20);

/// The review list, open and ready to walk.
#[derive(Debug)]
pub(crate) struct Opened {
	pub page_url: String,
	pub sorted: bool,
	pub total: Option<u64>,
	/// What went wrong on the way without failing it; the walk's warnings start with these.
	pub warnings: Vec<String>,
}

/// What the sort button opened.
enum SortMenu {
	Open,
	/// Google's sign-in dialog, in place of the menu.
	SignInRequired,
}

pub(crate) struct Session {
	browser: Browser,
	handler: JoinHandle<()>,
	page: Page,
	/// Where to save the cards' HTML on every walk, for refreshing test fixtures.
	dump_html: Option<PathBuf>,
	/// Released with the session, after the browser is closed.
	_profile: ProfileLock,
}

impl Session {
	/// Chromium on the profile `profile` holds.
	pub(crate) async fn launch(cfg: &BrowserConfig, profile_dir: &Path, profile: ProfileLock) -> Result<Self, SessionError> {
		let mut b = CdpConfig::builder()
			.user_data_dir(profile_dir)
			.new_headless_mode()
			// wide enough for the desktop layout, tall enough that a long review fits one screenshot
			.window_size(1280, 2000)
			.viewport(Viewport {
				width: 1280,
				height: 2000,
				device_scale_factor: Some(2.0),
				..Default::default()
			})
			// k8s gives /dev/shm 64 MB, which Chromium outgrows on a long feed
			.arg("--disable-dev-shm-usage")
			.launch_timeout(Duration::from_secs(60))
			.request_timeout(Duration::from_secs(60));
		if cfg.headful {
			b = b.with_head();
		}
		if cfg.no_sandbox {
			b = b.no_sandbox();
		}
		if let Some(exe) = &cfg.executable {
			b = b.chrome_executable(exe);
		}
		let config = b.build().map_err(|e| SessionError::new_launch(format!("browser config: {e}")))?;
		let (browser, mut handler) = Browser::launch(config).await.map_err(|e| SessionError::new_launch(e.to_string()))?;
		// The CDP event pump; it must run for any command to complete, and dies with the browser.
		let handler = tokio::spawn(async move {
			while let Some(event) = handler.next().await {
				if let Err(e) = event {
					tracing::debug!(error = %e, "CDP handler");
				}
			}
		});
		let page = browser.new_page("about:blank").await?;
		Ok(Self {
			browser,
			handler,
			page,
			dump_html: cfg.dump_html.clone(),
			_profile: profile,
		})
	}

	pub(crate) async fn close(mut self) {
		if let Err(e) = self.browser.close().await {
			tracing::warn!(error = %e, "closing Chromium");
		}
		// Reaping the child; a failure here only means it is already gone.
		if let Err(e) = self.browser.wait().await {
			tracing::debug!(error = %e, "waiting for Chromium to exit");
		}
		self.handler.abort();
	}

	/// Opens the place and its review list sorted newest first. `None`: the place has no
	/// reviews at all, so there is no list.
	pub(crate) async fn open_reviews(&self, place_id: &str, lang: &str) -> Result<Option<Opened>, SessionError> {
		let url = sel::place_url(place_id, lang);
		self.page.goto(url.as_str()).await?;
		self.handle_interstitials().await?;

		// The place panel renders after load; wait for the way into the reviews.
		if !self.wait_for_any(sel::REVIEWS_TAB).await? {
			if self.eval::<bool>(js::HAS_TEXT, (sel::LIMITED_VIEW_TEXT,)).await? {
				return Err(SessionError::new_limited_view());
			}
			// no place panel at all: Google does not know the id, whatever its markup is now
			if !self.eval::<bool>(js::ANY, (sel::PLACE_TITLE,)).await? {
				return Err(SessionError::new_place_not_found(place_id.to_owned()));
			}
			// The place rendered, and has no star average: nobody has reviewed it yet.
			if !self.eval::<bool>(js::ANY, (sel::RATING_SUMMARY,)).await? {
				tracing::info!(place_id, "the place has no reviews");
				return Ok(None);
			}
			return Err(markup_changed("waiting for the reviews tab", sel::REVIEWS_TAB));
		}
		if !self.click_first(sel::REVIEWS_TAB).await? {
			return Err(markup_changed("clicking the reviews tab", sel::REVIEWS_TAB));
		}
		if !self.wait_for_any(&[sel::CARD]).await? {
			if self.review_total().await? == Some(0) {
				tracing::info!(place_id, "the review list is empty");
				return Ok(None);
			}
			return Err(markup_changed("waiting for the review list", &[sel::CARD]));
		}
		let total = self.review_total().await?;

		if !self.wait_for_any(sel::SORT_BUTTON).await? {
			return Err(markup_changed("waiting for the sort button", sel::SORT_BUTTON));
		}
		let mut warnings = Vec::new();
		let sorted = match self.open_sort_menu().await? {
			SortMenu::Open => self.pick_newest().await?,
			SortMenu::SignInRequired => {
				self.dismiss_promo().await?;
				tracing::info!(place_id, "Google asks to sign in before sorting; reading the list in its own order");
				warnings.push(
					"Google asks this signed-out browser to sign in before it sorts the reviews or shows more than the first few; the ones shown were read in its own order".to_owned(),
				);
				false
			}
		};

		Ok(Some(Opened {
			page_url: self.page.url().await?.unwrap_or(url),
			sorted,
			total,
			warnings,
		}))
	}

	/// Clicks the sort button until the sort menu or Google's sign-in dialog shows.
	async fn open_sort_menu(&self) -> Result<SortMenu, SessionError> {
		let either = [sel::SORT_NEWEST, sel::SIGN_IN_GATE].concat();
		let mut gated = 0;
		// A click that lands while the list is still hydrating is swallowed; retry a few times.
		for _ in 0..4 {
			self.dismiss_promo().await?;
			if !self.click_first(sel::SORT_BUTTON).await? {
				return Err(markup_changed("clicking the sort button", sel::SORT_BUTTON));
			}
			if !self.wait_for_any_within(&either, Duration::from_secs(5)).await? {
				continue;
			}
			if self.eval::<bool>(js::ANY, (sel::SORT_NEWEST,)).await? {
				return Ok(SortMenu::Open);
			}
			// The same dialog also turns up by itself over a fresh profile's list; only a
			// second showing in answer to the button says the menu is gated.
			gated += 1;
			if gated == 2 {
				return Ok(SortMenu::SignInRequired);
			}
		}
		Err(markup_changed("opening the sort menu (or Google's sign-in dialog)", &either))
	}

	/// Picks "newest" in the open sort menu; `false`: the list was not seen to re-sort.
	async fn pick_newest(&self) -> Result<bool, SessionError> {
		let before: String = self.eval(js::FIRST_CARD_ID, (sel::CARD, sel::CARD_ID_ATTR)).await?;
		if !self.click_first(sel::SORT_NEWEST).await? {
			return Err(markup_changed("picking 'newest'", sel::SORT_NEWEST));
		}
		// The relevance-sorted cards stay in the DOM until the new list arrives; reading them
		// would archive the wrong end of the list. Wait for the first card to change.
		let deadline = tokio::time::Instant::now() + SORT_TIMEOUT;
		let sorted = loop {
			tokio::time::sleep(Duration::from_millis(300)).await;
			let now: String = self.eval(js::FIRST_CARD_ID, (sel::CARD, sel::CARD_ID_ATTR)).await?;
			if !now.is_empty() && now != before {
				break true;
			}
			if tokio::time::Instant::now() >= deadline {
				// The newest review can also be the most relevant one — or the click was
				// swallowed. The walk goes on, but its order is not relied on.
				tracing::info!("first card unchanged after sorting by newest; the list's order is unconfirmed");
				break false;
			}
		};
		tokio::time::sleep(STEP_WAIT).await;
		if !self.wait_for_any(&[sel::CARD]).await? {
			return Err(markup_changed("waiting for the list to come back sorted", &[sel::CARD]));
		}
		Ok(sorted)
	}

	/// The whole page as HTML and a full-page PNG, for seeing what a failed step was
	/// looking at: `page [<png>] [<html>]`, which the alerts attach.
	pub(crate) async fn save_page(&self, artifacts: &Artifacts) -> eyre::Result<String> {
		// A page that failed may also hang; a diagnostic is not worth holding the walk for.
		let (html, png) = tokio::time::timeout(DIAGNOSTIC_TIMEOUT, async {
			let html = self.page.content().await?;
			let png = self.page.screenshot(chromiumoxide::page::ScreenshotParams::builder().full_page(true).build()).await?;
			eyre::Ok((html, png))
		})
		.await
		.wrap_err("the page did not answer")??;
		let png = artifacts.save("page", "png", &png)?;
		let html = artifacts.save("page", "html", html.as_bytes())?;
		Ok(format!("page [{}] [{}]", png.display(), html.display()))
	}

	/// The review count the list's histogram adds up to.
	async fn review_total(&self) -> Result<Option<u64>, SessionError> {
		let labels: Vec<String> = self.eval(js::LABELS, (sel::HISTOGRAM_ROW,)).await?;
		Ok(parse::review_total(&labels))
	}

	/// Walks the open review list, capturing the cards the policy asks for. A page that
	/// fails once cards were read ends the walk [`WalkEnd::Interrupted`] with what it read
	/// — unless Google blocked it, which fails the walk.
	pub(crate) async fn walk(&self, policy: &mut dyn WalkPolicy, max: usize, opened: Opened) -> Result<Walked, SessionError> {
		let page_url = opened.page_url.as_str();
		let mut seen = HashSet::new();
		let mut cards: Vec<WalkedCard> = Vec::new();
		let mut warnings = opened.warnings;
		let mut idle = 0;
		let end = loop {
			let step = async {
				self.dismiss_promo().await?;
				self.eval::<u32>(js::EXPAND_ALL, (sel::CARD, sel::EXPAND)).await?;
				tokio::time::sleep(Duration::from_millis(300)).await;

				// Only the tail: cards already read are not re-parsed on every step. A screen of
				// overlap covers a feed that re-rendered its last few cards.
				let skip = seen.len().saturating_sub(SCREEN);
				let html: String = self.eval(js::CARDS_HTML, (sel::CARD, skip)).await?;
				if let Some(dir) = &self.dump_html {
					dump(dir, &html).await?;
				}
				let mut grew = false;
				for card in parse::cards(&html) {
					if cards.len() >= max {
						break;
					}
					if !seen.insert(card.id.clone()) {
						continue;
					}
					grew = true;
					let capture = if policy.wants_capture(&card) {
						self.capture(&card.id, page_url)
							.await
							.map_err(|e| warnings.push(format!("screenshot of {} failed: {e:#}", card.id)))
							.ok()
					} else {
						None
					};
					policy.observe(&card);
					cards.push((card, capture));
				}
				if cards.len() >= max {
					return Ok(Some(WalkEnd::Cap));
				}
				if policy.satisfied() {
					return Ok(Some(WalkEnd::Satisfied));
				}
				idle = if grew { 0 } else { idle + 1 };
				if idle >= END_AFTER_IDLE_STEPS {
					return Ok(Some(WalkEnd::ReachedEnd));
				}
				self.check_not_blocked().await?;
				// A list short enough to fit the panel has nothing to scroll and nothing more to load.
				if !self.eval::<bool>(js::SCROLL_FEED, (sel::CARD,)).await? {
					return Ok(Some(WalkEnd::ReachedEnd));
				}
				tokio::time::sleep(STEP_WAIT).await;
				Ok::<_, SessionError>(None)
			};
			match step.await {
				Ok(Some(end)) => break end,
				Ok(None) => {}
				Err(e) if cards.is_empty() || matches!(e, SessionError::Blocked { .. }) => return Err(e),
				Err(e) => {
					warnings.push(format!("the walk stopped after {} reviews: {e:#}", cards.len()));
					break WalkEnd::Interrupted;
				}
			}
		};
		Ok(Walked {
			cards,
			end,
			page_url: opened.page_url,
			warnings,
			sorted: opened.sorted,
			total: opened.total,
		})
	}

	async fn capture(&self, id: &str, page_url: &str) -> Result<Capture, SessionError> {
		let marked: bool = self.eval(js::MARK_CARD, (sel::CARD, sel::CARD_ID_ATTR, id)).await?;
		if !marked {
			return Err(eyre::eyre!("card is no longer on the page").into());
		}
		// let lazy avatars and photo thumbnails paint after the scroll
		tokio::time::sleep(Duration::from_millis(400)).await;
		let el = self.page.find_element(js::MARKED_CARD).await?;
		let png = el.screenshot(CaptureScreenshotFormat::Png).await?;
		Ok(Capture {
			png,
			captured_at: Timestamp::now(),
			page_url: page_url.to_owned(),
		})
	}

	async fn handle_interstitials(&self) -> Result<(), SessionError> {
		self.check_not_blocked().await?;
		let url = self.page.url().await?.unwrap_or_default();
		if !url.contains(sel::CONSENT_HOST) {
			return Ok(());
		}
		tracing::info!("consent page: rejecting all");
		if !self.wait_for_any(sel::CONSENT_REJECT).await? {
			return Err(SessionError::new_consent("no reject button"));
		}
		if !self.click_first(sel::CONSENT_REJECT).await? {
			return Err(SessionError::new_consent("could not click reject"));
		}
		// the answer is a cookie in the profile dir, so the next run goes straight through
		let deadline = tokio::time::Instant::now() + UI_TIMEOUT;
		while self.page.url().await?.unwrap_or_default().contains(sel::CONSENT_HOST) {
			if tokio::time::Instant::now() >= deadline {
				return Err(SessionError::new_consent("still on the consent page after rejecting"));
			}
			tokio::time::sleep(Duration::from_millis(300)).await;
		}
		self.check_not_blocked().await
	}

	/// Google's "unusual traffic" page: the run fails, whatever was read before it.
	async fn check_not_blocked(&self) -> Result<(), SessionError> {
		let url = self.page.url().await?.unwrap_or_default();
		if url.contains(sel::BLOCKED_PATH) {
			return Err(SessionError::new_blocked(url));
		}
		Ok(())
	}

	async fn dismiss_promo(&self) -> Result<(), SessionError> {
		if self.click_first(sel::PROMO_DISMISS).await? {
			tracing::debug!("dismissed the sign-in dialog");
			tokio::time::sleep(Duration::from_millis(500)).await;
		}
		Ok(())
	}

	async fn click_first(&self, sels: &[&str]) -> Result<bool, SessionError> {
		self.eval(js::CLICK_FIRST, (sels,)).await
	}

	/// Whether any of `sels` showed up in time.
	async fn wait_for_any(&self, sels: &[&str]) -> Result<bool, SessionError> {
		self.wait_for_any_within(sels, UI_TIMEOUT).await
	}

	async fn wait_for_any_within(&self, sels: &[&str], timeout: Duration) -> Result<bool, SessionError> {
		let deadline = tokio::time::Instant::now() + timeout;
		loop {
			if self.eval::<bool>(js::ANY, (sels,)).await? {
				return Ok(true);
			}
			self.check_not_blocked().await?;
			if tokio::time::Instant::now() >= deadline {
				return Ok(false);
			}
			tokio::time::sleep(Duration::from_millis(300)).await;
		}
	}

	/// Calls an in-page function with JSON-encoded arguments.
	async fn eval<T: serde::de::DeserializeOwned>(&self, func: &str, args: impl Serialize) -> Result<T, SessionError> {
		let args = serde_json::to_value(args)?;
		let args = args.as_array().map(|a| a.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")).unwrap_or_default();
		let expr = format!("({func})({args})");
		Ok(self.page.evaluate_expression(expr).await?.into_value()?)
	}
}

fn markup_changed(step: &'static str, sels: &[&str]) -> SessionError {
	SessionError::new_markup_changed(step, sels.iter().map(ToString::to_string).collect())
}

async fn dump(dir: &Path, html: &str) -> eyre::Result<()> {
	tokio::fs::create_dir_all(dir).await?;
	let path = dir.join(format!("cards-{}.html", Timestamp::now().as_millisecond()));
	tokio::fs::write(&path, format!("<div>\n{html}\n</div>\n"))
		.await
		.wrap_err_with(|| format!("writing {}", path.display()))
}
