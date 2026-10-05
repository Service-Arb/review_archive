//! What an [`Archive`](crate::Archive) is opened with. The browser and scan defaults
//! deserialize from a config file's sections; secrets are passed in, never read here.

use std::path::PathBuf;

use review_archive_core::{Known, check_lang, schedule::Schedule, tokens::Tokens};
use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;
use v_utils::Timeframe;

/// Everything [`Archive::open`](crate::Archive::open) needs.
#[derive(Clone, Debug, Default)]
pub struct Config {
	/// The archive's home: `review_archive.db`, `blobs/`, and the browser profile unless
	/// [`BrowserConfig::profile_dir`] says otherwise. `None`: no store, only
	/// [`Archive::capture_place`](crate::Archive::capture_place) works.
	pub data_dir: Option<PathBuf>,
	/// How to run Chromium.
	pub browser: BrowserConfig,
	/// Scan limits and what a new target defaults to.
	pub defaults: Defaults,
	/// How targets are paced.
	pub schedule: Schedule,
	/// What members' scans may spend.
	pub tokens: Tokens,
	/// Where webhooks may point.
	pub webhooks: WebhookConfig,
	/// API keys and tokens.
	pub secrets: Secrets,
}

impl Config {
	/// The SQLite file, under the data dir.
	pub fn db_path(&self) -> Option<PathBuf> {
		self.data_dir.as_ref().map(|d| d.join("review_archive.db"))
	}

	/// The PNG store, under the data dir.
	pub fn blob_dir(&self) -> Option<PathBuf> {
		self.data_dir.as_ref().map(|d| d.join("blobs"))
	}

	/// The browser profile: it is what remembers the consent answer between runs.
	pub fn profile_dir(&self) -> Option<PathBuf> {
		self.browser.profile_dir.clone().or_else(|| self.data_dir.as_ref().map(|d| d.join("chromium-profile")))
	}

	/// What alerts may attach: the page a failed walk was on, kept for a week.
	pub fn artifacts_dir(&self) -> Option<PathBuf> {
		self.data_dir.as_ref().map(|d| d.join("artifacts"))
	}
}

/// How to run Chromium, and how long a walk waits on Maps.
#[derive(Clone, Debug, Deserialize, Serialize, SmartDefault)]
#[cfg_attr(feature = "settings", derive(schemars::JsonSchema, v_utils::macros::SettingsNested), settings(prefix = "browser"))]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
	/// Chrome or Chromium binary; a walk fails without one.
	pub executable: Option<PathBuf>,
	/// Shows the window, for watching a scan by hand.
	pub headful: bool,
	/// Browser profile; `<data_dir>/chromium-profile` when unset.
	pub profile_dir: Option<PathBuf>,
	/// Between two scrolls of the review feed.
	#[default(Timeframe::from("1500ms"))]
	pub step_wait: Timeframe,
	/// Scrolls in a row without a new card before the feed counts as ended.
	#[default(5)]
	pub idle_steps_to_end: u32,
	/// For a control of the page to appear.
	#[default(Timeframe::from("20s"))]
	pub ui_timeout: Timeframe,
	/// For a page to load.
	#[default(Timeframe::from("1m"))]
	pub nav_timeout: Timeframe,
	/// For the list to re-sort to newest.
	#[default(Timeframe::from("10s"))]
	pub sort_timeout: Timeframe,
	/// How long the pages of failed walks are kept under [`Config::artifacts_dir`].
	#[default(Timeframe::from("1w"))]
	pub artifacts_retention: Timeframe,
	/// Saves the review cards' HTML of every step here, to refresh the parser fixtures.
	#[serde(skip)]
	#[cfg_attr(feature = "settings", settings(skip))]
	pub dump_html: Option<PathBuf>,
	/// A walk that fails saves the page here and its error says where, as `[<path>]`s.
	/// [`Archive::open`](crate::Archive::open) sets it under [`Config::artifacts_dir`]; `None` saves nothing.
	#[cfg(feature = "maps")]
	#[serde(skip)]
	#[cfg_attr(feature = "settings", settings(skip))]
	pub artifacts: Option<browser_manipulation::Artifacts>,
}

/// Scan limits and what a new target defaults to.
#[derive(Clone, Debug, Deserialize, Serialize, SmartDefault)]
#[cfg_attr(feature = "settings", derive(schemars::JsonSchema, v_utils::macros::SettingsNested))]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
	/// UI language of the Maps page for new targets.
	#[serde(deserialize_with = "de_lang")]
	#[default("en".to_owned())]
	pub lang: String,
	/// Scan interval for new targets; at least `schedule.min_interval`.
	#[default(Timeframe::from("1d"))]
	pub interval: Timeframe,
	/// Cards read on a scan of a target with an archive.
	#[default(200)]
	pub max_reviews_per_scan: usize,
	/// Cards read on a target's first scan, on a scan that fills a gap a previous one left,
	/// and by an ad-hoc capture at most.
	#[default(2000)]
	pub max_reviews_initial: usize,
	/// Jobs (scans now, ad-hoc captures) waiting for the browser at most; more are refused
	/// until some are done.
	#[default(20)]
	pub max_queued_jobs: usize,
	/// For one call to the Places or the Business Profile API: a hung one must not hold the one worker forever.
	#[default(Timeframe::from("15s"))]
	pub api_timeout: Timeframe,
}

impl Defaults {
	/// How many cards a scan against `known` reads at most.
	pub fn max_for(&self, known: &Known) -> usize {
		if known.initial || known.cut_after.is_some() {
			self.max_reviews_initial
		} else {
			self.max_reviews_per_scan
		}
	}
}

/// Where events may be sent, and how hard delivery tries.
#[derive(Clone, Debug, Deserialize, Serialize, SmartDefault)]
#[cfg_attr(feature = "settings", derive(schemars::JsonSchema, v_utils::macros::SettingsNested), settings(prefix = "webhooks"))]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
	/// The only hosts webhooks may point at, private addresses included (a service in the
	/// same cluster). Empty: any host, as long as every address it has is public — never
	/// loopback, private, link-local or the like.
	pub allowed_hosts: Vec<String>,
	/// The Telegram Bot API members' channels are posted through.
	#[default("https://api.telegram.org/".to_owned())]
	pub telegram_api: String,
	/// The wait after a failed try; doubles per failure in a row.
	#[default(Timeframe::from("30s"))]
	pub first_retry: Timeframe,
	/// The longest wait between two tries.
	#[default(Timeframe::from("6h"))]
	pub retry_cap: Timeframe,
	/// Tries before a delivery is given up on.
	#[default(12)]
	pub max_attempts: u32,
	/// Deliveries taken per pass.
	#[default(50)]
	pub batch: u32,
	/// Recipients delivered to at once: a slow one does not hold up the others.
	#[default(8)]
	pub parallel: usize,
	/// For a hook's address to answer.
	#[default(Timeframe::from("3s"))]
	pub connect_timeout: Timeframe,
	/// For a hook to acknowledge.
	#[default(Timeframe::from("10s"))]
	pub timeout: Timeframe,
	/// For the Bot API to acknowledge; a photo upload takes a while.
	#[default(Timeframe::from("30s"))]
	pub telegram_timeout: Timeframe,
}

/// API keys and tokens. Each is needed only by what uses it, and its absence fails only
/// that, with an error naming it.
#[derive(Clone, Default)]
pub struct Secrets {
	/// Places API key, for resolving a Maps URL that carries no place id
	/// (`GOOGLE_MAPS_KEY`).
	pub google_maps_key: Option<String>,
	/// OAuth credentials for `gbp` targets (`GBP_CLIENT_ID`, `GBP_CLIENT_SECRET`,
	/// `GBP_REFRESH_TOKEN`).
	#[cfg(feature = "maps")]
	pub gbp: Option<crate::sources::gbp::Credentials>,
	/// The bot members' Telegram channels are posted by (`TELEGRAM_BOT_TOKEN`).
	pub telegram_bot_token: Option<String>,
}

impl std::fmt::Debug for Secrets {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let mut d = f.debug_struct("Secrets");
		d.field("google_maps_key", &self.google_maps_key.as_ref().map(|_| "***"));
		#[cfg(feature = "maps")]
		d.field("gbp", &self.gbp.as_ref().map(|_| "***"));
		d.field("telegram_bot_token", &self.telegram_bot_token.as_ref().map(|_| "***"));
		d.finish()
	}
}

fn de_lang<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
	let s = String::deserialize(d)?;
	check_lang(&s).map_err(serde::de::Error::custom)?;
	Ok(s)
}
