//! What an [`Archive`](crate::Archive) is opened with. The browser and scan defaults
//! deserialize from a config file's sections; secrets are passed in, never read here.

use std::{path::PathBuf, time::Duration};

use review_archive_core::{Known, check_lang, parse_interval, schedule};
use serde::Deserialize;

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

	/// Where a failed walk saves the page it failed on.
	pub fn diagnostics_dir(&self) -> Option<PathBuf> {
		self.browser.diagnostics_dir.clone().or_else(|| self.data_dir.as_ref().map(|d| d.join("diagnostics")))
	}
}

/// How to run Chromium.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
	/// Chrome or Chromium binary; found on PATH when unset.
	pub executable: Option<PathBuf>,
	/// For containers that run as an unprivileged user without user namespaces.
	pub no_sandbox: bool,
	/// Shows the window, for watching a scan by hand.
	pub headful: bool,
	/// Browser profile; `<data_dir>/chromium-profile` when unset.
	pub profile_dir: Option<PathBuf>,
	/// Saves the review cards' HTML of every step here, to refresh the parser fixtures.
	#[serde(skip)]
	pub dump_html: Option<PathBuf>,
	/// A walk that fails saves the page here, as `<UTC time>-<place id>.{png,html}`, and its
	/// error says where. [`Archive::open`](crate::Archive::open) makes it
	/// `<data_dir>/diagnostics` when unset; a `Browser` built by hand on `None`
	/// saves nothing.
	#[serde(skip)]
	pub diagnostics_dir: Option<PathBuf>,
}

/// Scan limits and what a new target defaults to.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
	/// UI language of the Maps page for new targets.
	#[serde(deserialize_with = "de_lang")]
	pub lang: String,
	/// Scan interval for new targets; at least an hour.
	#[serde(deserialize_with = "de_interval")]
	pub interval: Duration,
	/// Cards read on a scan of a target with an archive.
	pub max_reviews_per_scan: usize,
	/// Cards read on a target's first scan, on a scan that fills a gap a previous one left,
	/// and by an ad-hoc capture at most.
	pub max_reviews_initial: usize,
	/// Jobs (scans now, ad-hoc captures) waiting for the browser at most; more are refused
	/// until some are done.
	pub max_queued_jobs: usize,
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

impl Default for Defaults {
	fn default() -> Self {
		Self {
			lang: "en".into(),
			interval: schedule::DEFAULT_INTERVAL,
			max_reviews_per_scan: 200,
			max_reviews_initial: 2000,
			max_queued_jobs: 20,
		}
	}
}

/// Where webhooks may be sent.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
	/// The only hosts webhooks may point at, private addresses included (a service in the
	/// same cluster). Empty: any host, as long as every address it has is public — never
	/// loopback, private, link-local or the like.
	pub allowed_hosts: Vec<String>,
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
}

impl std::fmt::Debug for Secrets {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let mut d = f.debug_struct("Secrets");
		d.field("google_maps_key", &self.google_maps_key.as_ref().map(|_| "***"));
		#[cfg(feature = "maps")]
		d.field("gbp", &self.gbp.as_ref().map(|_| "***"));
		d.finish()
	}
}

fn de_interval<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
	let s = String::deserialize(d)?;
	parse_interval(&s).map_err(serde::de::Error::custom)
}

fn de_lang<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
	let s = String::deserialize(d)?;
	check_lang(&s).map_err(serde::de::Error::custom)?;
	Ok(s)
}
