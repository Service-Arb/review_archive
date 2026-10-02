//! Driving a plain headless Chromium. No stealth, no fingerprint masking:
//! the scanner is what it looks like, and a block from Google fails the run.

use std::{collections::HashSet, path::Path, time::Duration};

use browser_manipulation::{Browser, ErrorKind, Launch, Robot, Shot, Tab, Viewport};
use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{
	Capture,
	maps::{
		SCREEN, WalkEnd, WalkPolicy, Walked, WalkedCard, cost, parse,
		selectors::{self as sel, js},
	},
	tokens::Meter,
};
use serde::Serialize;

use crate::{SessionError, config::BrowserConfig};

/// The review list, open and ready to walk.
#[derive(Debug)]
pub(crate) struct Opened {
	pub page_url: String,
	pub sorted: bool,
	pub total: Option<u64>,
	pub post: Option<parse::Post>,
}

/// What the sort button opened.
enum SortMenu {
	Open,
	/// Google's sign-in dialog, in place of the menu.
	SignInRequired,
}

pub(crate) struct Session {
	browser: Browser<Robot>,
	cfg: BrowserConfig,
}

/// One walk's tab.
pub(crate) struct Page<'s> {
	tab: Tab<'s, Robot>,
	cfg: &'s BrowserConfig,
}

impl Session {
	pub(crate) async fn launch(cfg: &BrowserConfig, profile_dir: &Path) -> Result<Self, SessionError> {
		let executable = cfg.executable.clone().ok_or_else(|| SessionError::new_launch("`browser.executable` is not set".to_owned()))?;
		let launch = Launch::Owned {
			profile: profile_dir.to_owned(),
			executable,
			headless: !cfg.headful,
			// wide enough for the desktop layout, tall enough that a long review fits one screenshot
			viewport: Some(Viewport {
				width: 1280,
				height: 2000,
				device_scale_factor: 2.0, // card PNGs are archived evidence; 1x is too blurry to read
			}),
		};
		let browser = Browser::launch(launch, Robot, cfg.artifacts.clone()).await?;
		Ok(Self { browser, cfg: cfg.clone() })
	}

	pub(crate) async fn close(self) {
		if let Err(e) = self.browser.close().await {
			tracing::warn!(error = %e, "closing Chromium");
		}
	}

	pub(crate) async fn page(&self) -> Result<Page<'_>, SessionError> {
		Ok(Page {
			tab: self.browser.tab().await?,
			cfg: &self.cfg,
		})
	}
}

impl Page<'_> {
	pub(crate) async fn close(self) -> Result<(), SessionError> {
		Ok(self.tab.close().await?)
	}

	/// Opens the place, reads the owner's latest post off its overview, and opens its review
	/// list sorted newest first. `Err`: the walk is already over — the place has no reviews,
	/// or `policy` does not want them read.
	pub(crate) async fn open_reviews(&mut self, place_id: &str, lang: &str, policy: &mut dyn WalkPolicy, meter: &mut Meter) -> Result<Result<Opened, Walked>, SessionError> {
		let url = sel::place_url(place_id, lang);
		self.tab.set_timeout(self.cfg.nav_timeout.duration()).await;
		meter.spend(cost::OPEN);
		self.tab.goto(&url).await?;
		self.handle_interstitials().await?;

		// The place panel renders after load; wait for the way into the reviews.
		let has_reviews_tab = self.wait_for_any(sel::REVIEWS_TAB).await?;
		if self.eval::<bool>(js::ANY, sel::SIGNED_OUT).await? {
			return Err(SessionError::new_signed_out());
		}
		let post = parse::owner_post(&self.eval::<String>(js::FIRST_HTML, sel::OWNER_POST).await?);
		if !has_reviews_tab {
			if self.eval::<bool>(js::HAS_TEXT, sel::LIMITED_VIEW_TEXT).await? {
				return Err(SessionError::new_limited_view());
			}
			// no place panel at all: Google does not know the id, whatever its markup is now
			if !self.eval::<bool>(js::ANY, sel::PLACE_TITLE).await? {
				return Err(SessionError::new_place_not_found(place_id.to_owned()));
			}
			// The place rendered, and has no star average: nobody has reviewed it yet.
			if !self.eval::<bool>(js::ANY, sel::RATING_SUMMARY).await? {
				tracing::info!(place_id, "the place has no reviews");
				return Ok(Err(Walked::empty(url, post)));
			}
			return Err(markup_changed("waiting for the reviews tab", sel::REVIEWS_TAB));
		}
		meter.spend(cost::REVIEWS);
		if !self.click_first(sel::REVIEWS_TAB).await? {
			return Err(markup_changed("clicking the reviews tab", sel::REVIEWS_TAB));
		}
		if !self.wait_for_any(&[sel::CARD]).await? {
			if self.review_total().await? == Some(0) {
				tracing::info!(place_id, "the review list is empty");
				return Ok(Err(Walked::empty(url, post)));
			}
			return Err(markup_changed("waiting for the review list", &[sel::CARD]));
		}
		let total = self.review_total().await?;
		if !policy.wants_list(total) {
			tracing::info!(place_id, ?total, "the review count is the last scan's; the list is not read");
			return Ok(Err(Walked::unchanged(self.tab.url(), total, post)));
		}

		if !self.wait_for_any(sel::SORT_BUTTON).await? {
			return Err(markup_changed("waiting for the sort button", sel::SORT_BUTTON));
		}
		let sorted = match self.open_sort_menu(meter).await? {
			SortMenu::Open => self.pick_newest().await?,
			SortMenu::SignInRequired => return Err(SessionError::new_signed_out()),
		};

		Ok(Ok(Opened {
			page_url: self.tab.url(),
			sorted,
			total,
			post,
		}))
	}

	/// Clicks the sort button until the sort menu or Google's sign-in dialog shows.
	async fn open_sort_menu(&mut self, meter: &mut Meter) -> Result<SortMenu, SessionError> {
		let either = [sel::SORT_NEWEST, sel::SIGN_IN_GATE].concat();
		let mut gated = 0;
		// A click that lands while the list is still hydrating is swallowed; retry a few times.
		for attempt in 0..4 {
			meter.spend(if attempt == 0 { cost::SORT } else { cost::SORT_RETRY });
			self.dismiss_promo().await?;
			if !self.click_first(sel::SORT_BUTTON).await? {
				return Err(markup_changed("clicking the sort button", sel::SORT_BUTTON));
			}
			if !self.wait_for_any_within(&either, Duration::from_secs(5)).await? {
				continue;
			}
			if self.eval::<bool>(js::ANY, sel::SORT_NEWEST).await? {
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
	async fn pick_newest(&mut self) -> Result<bool, SessionError> {
		let before: String = self.eval(js::FIRST_CARD_ID, (sel::CARD, sel::CARD_ID_ATTR)).await?;
		if !self.click_first(sel::SORT_NEWEST).await? {
			return Err(markup_changed("picking 'newest'", sel::SORT_NEWEST));
		}
		// The relevance-sorted cards stay in the DOM until the new list arrives; reading them
		// would archive the wrong end of the list. Wait for the first card to change.
		let deadline = tokio::time::Instant::now() + self.cfg.sort_timeout.duration();
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
		tokio::time::sleep(self.cfg.step_wait.duration()).await;
		if !self.wait_for_any(&[sel::CARD]).await? {
			return Err(markup_changed("waiting for the list to come back sorted", &[sel::CARD]));
		}
		Ok(sorted)
	}

	/// The page as it is now, into the artifacts; `None` without them.
	pub(crate) async fn save(&self, hint: &str) -> Option<Result<browser_manipulation::Capture, String>> {
		self.tab.capture(hint).await
	}

	/// The review count the list's histogram adds up to.
	async fn review_total(&mut self) -> Result<Option<u64>, SessionError> {
		let labels: Vec<String> = self.eval(js::LABELS, sel::HISTOGRAM_ROW).await?;
		Ok(parse::review_total(&labels))
	}

	/// Walks the open review list, capturing the cards the policy asks for. A page that
	/// fails once cards were read ends the walk [`WalkEnd::Interrupted`] with what it read
	/// — unless Google blocked it, which fails the walk.
	pub(crate) async fn walk(&mut self, policy: &mut dyn WalkPolicy, meter: &mut Meter, max: usize, opened: Opened) -> Result<Walked, SessionError> {
		let page_url = opened.page_url.as_str();
		let mut seen = HashSet::new();
		let mut cards: Vec<WalkedCard> = Vec::new();
		let mut warnings = Vec::new();
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
				if let Some(dir) = self.cfg.dump_html.as_deref() {
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
				if idle >= self.cfg.idle_steps_to_end {
					return Ok(Some(WalkEnd::ReachedEnd));
				}
				self.check_not_blocked()?;
				if !meter.try_spend(cost::STEP) {
					return Ok(Some(WalkEnd::Budget));
				}
				// A list short enough to fit the panel has nothing to scroll and nothing more to load.
				if !self.eval::<bool>(js::SCROLL_FEED, sel::CARD).await? {
					return Ok(Some(WalkEnd::ReachedEnd));
				}
				tokio::time::sleep(self.cfg.step_wait.duration()).await;
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
			post: opened.post,
		})
	}

	async fn capture(&mut self, id: &str, page_url: &str) -> Result<Capture, SessionError> {
		let marked: bool = self.eval(js::MARK_CARD, (sel::CARD, sel::CARD_ID_ATTR, id)).await?;
		if !marked {
			return Err(eyre::eyre!("card is no longer on the page").into());
		}
		// let lazy avatars and photo thumbnails paint after the scroll
		tokio::time::sleep(Duration::from_millis(400)).await;
		let png = self.tab.screenshot(Shot::Element(js::MARKED_CARD)).await?;
		Ok(Capture {
			png,
			captured_at: Timestamp::now(),
			page_url: page_url.to_owned(),
		})
	}

	async fn handle_interstitials(&mut self) -> Result<(), SessionError> {
		self.check_not_blocked()?;
		if !self.tab.url().contains(sel::CONSENT_HOST) {
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
		let deadline = tokio::time::Instant::now() + self.cfg.ui_timeout.duration();
		while self.tab.url().contains(sel::CONSENT_HOST) {
			if tokio::time::Instant::now() >= deadline {
				return Err(SessionError::new_consent("still on the consent page after rejecting"));
			}
			tokio::time::sleep(Duration::from_millis(300)).await;
		}
		self.check_not_blocked()
	}

	/// Google's "unusual traffic" page: the run fails, whatever was read before it.
	fn check_not_blocked(&self) -> Result<(), SessionError> {
		let url = self.tab.url();
		if url.contains(sel::BLOCKED_PATH) {
			return Err(SessionError::new_blocked(url));
		}
		Ok(())
	}

	async fn dismiss_promo(&mut self) -> Result<(), SessionError> {
		if self.click_first(sel::PROMO_DISMISS).await? {
			tracing::debug!("dismissed the sign-in dialog");
			tokio::time::sleep(Duration::from_millis(500)).await;
		}
		Ok(())
	}

	async fn click_first(&mut self, sels: &[&str]) -> Result<bool, SessionError> {
		self.eval(js::CLICK_FIRST, sels).await
	}

	/// Whether any of `sels` showed up in time.
	async fn wait_for_any(&mut self, sels: &[&str]) -> Result<bool, SessionError> {
		self.wait_for_any_within(sels, self.cfg.ui_timeout.duration()).await
	}

	async fn wait_for_any_within(&mut self, sels: &[&str], timeout: Duration) -> Result<bool, SessionError> {
		self.tab.set_timeout(timeout).await;
		let shown = self.tab.wait_for_any(sels).await;
		self.tab.set_timeout(self.cfg.ui_timeout.duration()).await;
		match shown {
			Ok(_) => Ok(true),
			// what a timed-out page showed was captured and is dropped here: the caller decides whether not showing up is a failure
			Err(e) if matches!(*e.kind, ErrorKind::Timeout { .. }) => {
				self.check_not_blocked()?;
				Ok(false)
			}
			Err(e) => Err(e.into()),
		}
	}

	async fn eval<T: serde::de::DeserializeOwned>(&mut self, func: &str, arg: impl Serialize) -> Result<T, SessionError> {
		Ok(self.tab.eval(func, arg).await?)
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
