//! Driving a plain headless Chromium over CDP. No stealth, no fingerprint masking:
//! the scanner is what it looks like, and a block from Google fails the run.

use std::{
	collections::HashSet,
	path::{Path, PathBuf},
	time::Duration,
};

use chromiumoxide::{Browser, BrowserConfig as CdpConfig, Page, cdp::browser_protocol::page::CaptureScreenshotFormat, handler::viewport::Viewport};
use eyre::WrapErr;
use futures::StreamExt;
use jiff::Timestamp;
use serde::Serialize;
use tokio::task::JoinHandle;

use super::{
	parse::{self, Card},
	selectors::{self as sel, js},
};
use crate::{config::BrowserConfig, domain::Capture};

/// Cards per screen of the feed: a run this long of already-archived cards ends a walk.
pub const SCREEN: usize = 10;
const STEP_WAIT: Duration = Duration::from_millis(1500);
/// Consecutive scrolls without a new card before the feed counts as ended.
const END_AFTER_IDLE_STEPS: u32 = 5;
const UI_TIMEOUT: Duration = Duration::from_secs(20);
const SORT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a walk needs to know about each card, and when it has seen enough.
pub trait WalkPolicy {
	fn is_known(&self, card: &Card) -> bool;
	fn wants_capture(&self, card: &Card) -> bool;
	/// Checked after each step; `true` ends the walk early.
	fn satisfied(&self, cards: &[Card]) -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum WalkEnd {
	/// Scrolling stopped producing cards: the whole list was seen.
	ReachedEnd,
	/// The policy had what it needed.
	Satisfied,
	/// `max` cards were read.
	Cap,
}

#[derive(Debug)]
pub struct Walked {
	pub cards: Vec<(Card, Option<Capture>)>,
	pub end: WalkEnd,
	pub page_url: String,
	pub warnings: Vec<String>,
}

pub struct Session {
	browser: Browser,
	handler: JoinHandle<()>,
	page: Page,
	/// Where to save the cards' HTML on every walk, for refreshing test fixtures.
	pub dump_html: Option<PathBuf>,
}

impl Session {
	pub async fn launch(cfg: &BrowserConfig, profile_dir: &Path) -> eyre::Result<Self> {
		tokio::fs::create_dir_all(profile_dir).await.wrap_err_with(|| format!("creating {}", profile_dir.display()))?;
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
		let config = b.build().map_err(|e| eyre::eyre!("browser config: {e}"))?;
		let (browser, mut handler) = Browser::launch(config).await.wrap_err("launching Chromium")?;
		// The CDP event pump; it must run for any command to complete, and dies with the browser.
		let handler = tokio::spawn(async move {
			while let Some(event) = handler.next().await {
				if let Err(e) = event {
					tracing::debug!(error = %e, "CDP handler");
				}
			}
		});
		let page = browser.new_page("about:blank").await.wrap_err("opening a tab")?;
		Ok(Self {
			browser,
			handler,
			page,
			dump_html: None,
		})
	}

	pub async fn close(mut self) {
		if let Err(e) = self.browser.close().await {
			tracing::warn!(error = %e, "closing Chromium");
		}
		// Reaping the child; a failure here only means it is already gone.
		if let Err(e) = self.browser.wait().await {
			tracing::debug!(error = %e, "waiting for Chromium to exit");
		}
		self.handler.abort();
	}

	/// Opens the place and its review list sorted newest first. Returns the page URL.
	pub async fn open_reviews(&self, place_id: &str, lang: &str) -> eyre::Result<String> {
		let res = self.open_reviews_inner(place_id, lang).await;
		if res.is_err()
			&& let Some(dir) = &self.dump_html
		{
			self.dump_page(dir).await;
		}
		res
	}

	/// The whole page and a screenshot of it, for seeing what a failed step was looking at.
	async fn dump_page(&self, dir: &Path) {
		let stamp = Timestamp::now().as_millisecond();
		let html = self.page.content().await.map_err(eyre::Report::new);
		let shot = self.page.screenshot(chromiumoxide::page::ScreenshotParams::builder().build()).await.map_err(eyre::Report::new);
		let written = async {
			tokio::fs::create_dir_all(dir).await?;
			tokio::fs::write(dir.join(format!("page-{stamp}.html")), html?).await?;
			tokio::fs::write(dir.join(format!("page-{stamp}.png")), shot?).await?;
			eyre::Ok(())
		};
		match written.await {
			Ok(()) => tracing::info!(dir = %dir.display(), "dumped the page"),
			Err(e) => tracing::warn!(error = %format!("{e:#}"), "could not dump the page"),
		}
	}

	async fn open_reviews_inner(&self, place_id: &str, lang: &str) -> eyre::Result<String> {
		let url = sel::place_url(place_id, lang);
		self.page.goto(url.as_str()).await.wrap_err_with(|| format!("loading {url}"))?;
		self.handle_interstitials().await?;

		// The place panel renders after load; wait for the way into the reviews.
		if let Err(e) = self.wait_for_any(sel::REVIEWS_TAB).await {
			let limited: bool = self.eval(js::HAS_TEXT, (sel::LIMITED_VIEW_TEXT,)).await?;
			eyre::ensure!(
				!limited,
				"Google served its \"limited view\" of Maps, which has no reviews: this browser session is not trusted with the full page"
			);
			return Err(e.wrap_err("no reviews tab on the place page (no reviews yet, or the markup changed)"));
		}
		eyre::ensure!(self.click_first(sel::REVIEWS_TAB).await?, "could not click the reviews tab");
		self.wait_for_any(&[sel::CARD]).await.wrap_err("the review list did not appear")?;

		self.wait_for_any(sel::SORT_BUTTON).await.wrap_err("no sort button on the review list")?;
		self.dismiss_promo().await?;
		// A click that lands while the list is still hydrating is swallowed; retry a few times.
		let mut menu_open = false;
		for _ in 0..4 {
			self.dismiss_promo().await?;
			eyre::ensure!(self.click_first(sel::SORT_BUTTON).await?, "could not open the sort menu");
			if self.wait_for_any_within(sel::SORT_NEWEST, Duration::from_secs(5)).await.is_ok() {
				menu_open = true;
				break;
			}
		}
		eyre::ensure!(menu_open, "the sort menu has no 'newest' entry (or would not open)");
		let before: String = self.eval(js::FIRST_CARD_ID, (sel::CARD, sel::CARD_ID_ATTR)).await?;
		eyre::ensure!(self.click_first(sel::SORT_NEWEST).await?, "could not pick 'newest'");
		// The relevance-sorted cards stay in the DOM until the new list arrives; reading them
		// would archive the wrong end of the list. Wait for the first card to change.
		let deadline = tokio::time::Instant::now() + SORT_TIMEOUT;
		loop {
			tokio::time::sleep(Duration::from_millis(300)).await;
			let now: String = self.eval(js::FIRST_CARD_ID, (sel::CARD, sel::CARD_ID_ATTR)).await?;
			if !now.is_empty() && now != before {
				break;
			}
			if tokio::time::Instant::now() >= deadline {
				// the newest review can also be the most relevant one
				tracing::info!("first card unchanged after sorting by newest; taking the list as sorted");
				break;
			}
		}
		tokio::time::sleep(STEP_WAIT).await;
		self.wait_for_any(&[sel::CARD]).await.wrap_err("the review list did not come back after sorting")?;

		Ok(self.page.url().await?.unwrap_or(url))
	}

	/// Walks the open review list, capturing the cards the policy asks for.
	pub async fn walk(&self, policy: &(dyn WalkPolicy + Sync), max: usize, page_url: &str) -> eyre::Result<Walked> {
		let mut seen = HashSet::new();
		let mut cards: Vec<Card> = Vec::new();
		let mut captures = Vec::new();
		let mut warnings = Vec::new();
		let mut idle = 0;
		let end = loop {
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
					match self.capture(&card.id, page_url).await {
						Ok(c) => Some(c),
						Err(e) => {
							warnings.push(format!("screenshot of {} failed: {e:#}", card.id));
							None
						}
					}
				} else {
					None
				};
				cards.push(card);
				captures.push(capture);
			}
			if cards.len() >= max {
				break WalkEnd::Cap;
			}
			if policy.satisfied(&cards) {
				break WalkEnd::Satisfied;
			}
			idle = if grew { 0 } else { idle + 1 };
			if idle >= END_AFTER_IDLE_STEPS {
				break WalkEnd::ReachedEnd;
			}
			self.check_not_blocked().await?;
			let scrolled: bool = self.eval(js::SCROLL_FEED, (sel::CARD,)).await?;
			eyre::ensure!(scrolled, "found no scrollable review feed");
			tokio::time::sleep(STEP_WAIT).await;
		};
		Ok(Walked {
			cards: cards.into_iter().zip(captures).collect(),
			end,
			page_url: page_url.to_owned(),
			warnings,
		})
	}

	async fn capture(&self, id: &str, page_url: &str) -> eyre::Result<Capture> {
		let marked: bool = self.eval(js::MARK_CARD, (sel::CARD, sel::CARD_ID_ATTR, id)).await?;
		eyre::ensure!(marked, "card is no longer on the page");
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

	async fn handle_interstitials(&self) -> eyre::Result<()> {
		self.check_not_blocked().await?;
		let url = self.page.url().await?.unwrap_or_default();
		if !url.contains(sel::CONSENT_HOST) {
			return Ok(());
		}
		tracing::info!("consent page: rejecting all");
		self.wait_for_any(sel::CONSENT_REJECT).await.wrap_err("consent page without a reject button")?;
		eyre::ensure!(self.click_first(sel::CONSENT_REJECT).await?, "could not click reject on the consent page");
		// the answer is a cookie in the profile dir, so the next run goes straight through
		let deadline = tokio::time::Instant::now() + UI_TIMEOUT;
		while self.page.url().await?.unwrap_or_default().contains(sel::CONSENT_HOST) {
			eyre::ensure!(tokio::time::Instant::now() < deadline, "still on the consent page after rejecting");
			tokio::time::sleep(Duration::from_millis(300)).await;
		}
		self.check_not_blocked().await
	}

	async fn check_not_blocked(&self) -> eyre::Result<()> {
		let url = self.page.url().await?.unwrap_or_default();
		eyre::ensure!(!url.contains(sel::BLOCKED_PATH), "blocked by Google (\"unusual traffic\" page at {url})");
		Ok(())
	}

	async fn dismiss_promo(&self) -> eyre::Result<()> {
		if self.click_first(sel::PROMO_DISMISS).await? {
			tracing::debug!("dismissed the sign-in dialog");
			tokio::time::sleep(Duration::from_millis(500)).await;
		}
		Ok(())
	}

	async fn click_first(&self, sels: &[&str]) -> eyre::Result<bool> {
		self.eval(js::CLICK_FIRST, (sels,)).await
	}

	async fn wait_for_any(&self, sels: &[&str]) -> eyre::Result<()> {
		self.wait_for_any_within(sels, UI_TIMEOUT).await
	}

	async fn wait_for_any_within(&self, sels: &[&str], timeout: Duration) -> eyre::Result<()> {
		let deadline = tokio::time::Instant::now() + timeout;
		loop {
			if self.eval::<bool>(js::ANY, (sels,)).await? {
				return Ok(());
			}
			self.check_not_blocked().await?;
			eyre::ensure!(tokio::time::Instant::now() < deadline, "timed out waiting for any of {sels:?}");
			tokio::time::sleep(Duration::from_millis(300)).await;
		}
	}

	/// Calls an in-page function with JSON-encoded arguments.
	async fn eval<T: serde::de::DeserializeOwned>(&self, func: &str, args: impl Serialize) -> eyre::Result<T> {
		let args = serde_json::to_value(args)?;
		let args = args.as_array().map(|a| a.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")).unwrap_or_default();
		let expr = format!("({func})({args})");
		let res = self.page.evaluate_expression(expr).await.wrap_err("evaluating in page")?;
		res.into_value().wrap_err("decoding in-page result")
	}
}

async fn dump(dir: &Path, html: &str) -> eyre::Result<()> {
	tokio::fs::create_dir_all(dir).await?;
	let path = dir.join(format!("cards-{}.html", Timestamp::now().as_millisecond()));
	tokio::fs::write(&path, format!("<div>\n{html}\n</div>\n"))
		.await
		.wrap_err_with(|| format!("writing {}", path.display()))
}
