//! What a review archive is, with no browser, database, network or clock in it.
//!
//! This crate is for anyone who has the HTML of a Google Maps review list, or a
//! list of reviews from anywhere, and wants to parse it and decide what is new,
//! changed or gone — without taking on a browser or a database. The engine crate
//! (`review_archive`) and the HTTP client (`review_archive_client`) are built on it.
//!
//! - [`maps::parse::cards`] reads review cards out of Maps HTML;
//!   [`maps::selectors`] is every assumption about that markup.
//! - [`reconcile::plan`] compares a [`Scan`] with what is [`Known`] and says
//!   what is new, changed, unchanged, reappeared and gone.
//! - [`relative_date::estimate`] turns "il y a 3 semaines" into a timestamp.
//! - [`schedule`] decides when a target is next due.
//! - [`dto`] holds the JSON shapes of the HTTP API.
//!
//! Time always comes in as an argument; nothing here reads the clock.
//!
//! ```
//! use review_archive_core::{maps, reconcile, Coverage, Known, Scan};
//!
//! let html = r#"<div data-review-id="r1" aria-label="Ann">
//!   <span role="img" aria-label="5 stars"></span><span class="rsqaWe">a week ago</span>
//!   <div class="MyEned"><span class="wiI7pd">Lovely</span></div></div>"#;
//! let now = "2026-09-26T12:00:00Z".parse().unwrap();
//! let reviews = maps::parse::cards(html).into_iter().map(|c| maps::observed(c, now)).collect();
//! let scan = Scan { reviews, coverage: Coverage::DownTo(None), warnings: vec![], cut_after: None };
//! let plan = reconcile::plan(&Known::default(), &scan);
//! assert_eq!(plan.new.len(), 1);
//! assert_eq!(plan.new[0].rating, Some(5));
//! ```

#![warn(missing_docs)]

pub mod dto;
pub mod gbp;
pub mod maps;
pub mod place;
pub mod reconcile;
pub mod relative_date;
pub mod schedule;

use std::{collections::HashMap, fmt, str::FromStr, time::Duration};

use jiff::Timestamp;
use sha2::{Digest, Sha256};

/// A watched place's id in the archive.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TargetId(pub i64);

impl fmt::Display for TargetId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.fmt(f)
	}
}

/// A review's id in the archive (not the source's id for it).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewId(pub i64);

impl fmt::Display for ReviewId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.fmt(f)
	}
}

/// A request the archive turns down: the caller's mistake, not the archive's. It travels
/// inside `eyre::Report` like any other error; whoever answers requests finds it with
/// `downcast_ref::<Rejected>()` and says 404, 400 or 429 instead of 500.
#[derive(Clone, Debug, Eq, PartialEq, miette::Diagnostic, thiserror::Error)]
pub enum Rejected {
	/// What was named does not exist.
	#[error("{0}")]
	#[diagnostic(code(review_archive::rejected::not_found))]
	NotFound(String),
	/// The input cannot be used; the message says why.
	#[error("{0}")]
	#[diagnostic(code(review_archive::rejected::invalid))]
	Invalid(String),
	/// Too much is already waiting, or the source is paused; asking again later can work.
	#[error("{0}")]
	#[diagnostic(code(review_archive::rejected::busy))]
	Busy(String),
}

impl Rejected {
	/// [`Self::Invalid`].
	pub fn invalid(msg: impl Into<String>) -> Self {
		Self::Invalid(msg.into())
	}

	/// [`Self::NotFound`].
	pub fn not_found(msg: impl Into<String>) -> Self {
		Self::NotFound(msg.into())
	}
}

/// Where a target's reviews are read from: `maps` or `gbp`, as stored and as the API
/// spells it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::AsRefStr, serde::Deserialize, strum::EnumString, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum TargetKind {
	/// The public Google Maps page, in a browser.
	Maps,
	/// A Business Profile we manage, through the official API.
	Gbp,
}

/// The `accounts/{account}/locations/{location}` pair of a Business Profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GbpLocation {
	/// The numeric account id.
	pub account: String,
	/// The numeric location id.
	pub location: String,
}

impl FromStr for GbpLocation {
	type Err = Rejected;

	/// Both ids are numbers: they become path segments of the API's URL.
	fn from_str(s: &str) -> Result<Self, Rejected> {
		let s = s.trim().trim_start_matches("accounts/");
		let bad = || Rejected::invalid(format!("gbp: expected <account>/<location>, two numbers, got {s:?}"));
		let (account, location) = s.split_once('/').ok_or_else(bad)?;
		let location = location.trim_start_matches("locations/");
		let numeric = |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
		if !numeric(account) || !numeric(location) {
			return Err(bad());
		}
		Ok(Self {
			account: account.to_owned(),
			location: location.to_owned(),
		})
	}
}

/// A watched place.
#[derive(Clone, Debug)]
pub struct Target {
	/// Its id in the archive.
	pub id: TargetId,
	/// What people call it; the place's name unless given.
	pub label: String,
	/// Where its reviews are read from.
	pub kind: TargetKind,
	/// The Google place id (`ChIJ…`). `gbp` targets need it too: screenshots come from Maps.
	pub place_id: String,
	/// Set exactly when `kind` is `Gbp`.
	pub gbp: Option<GbpLocation>,
	/// UI language of the Maps page, which is the language of its relative dates.
	pub lang: String,
	/// How often it is scanned, before jitter.
	pub interval: Duration,
	/// Disabled targets are kept, with their archive, but not scanned.
	pub enabled: bool,
	/// When it was added.
	pub created_at: Timestamp,
}

/// One review as a source saw it on this scan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observed {
	/// The source's own id for the review.
	pub source_review_id: String,
	/// The reviewer's display name.
	pub author: String,
	/// The reviewer's public profile, when the source links one.
	pub author_url: Option<String>,
	/// 1–5 stars.
	pub rating: Option<u8>,
	/// The review text; `None` for a rating without words.
	pub text: Option<String>,
	/// The owner's response.
	pub reply: Option<String>,
	/// Photos attached to the review.
	pub photo_count: u32,
	/// The date as the source prints it: "3 weeks ago", or an RFC 3339 time.
	pub published_raw: Option<String>,
	/// When it was published, as well as `published_raw` says.
	pub published_est: Option<Timestamp>,
	/// A screenshot of it, when one was taken on this scan.
	pub capture: Option<Capture>,
}

impl Observed {
	/// See [`content_hash`].
	pub fn content_hash(&self) -> String {
		content_hash(self.rating, self.text.as_deref(), self.reply.as_deref())
	}
}

/// A PNG of the review card, straight from the browser.
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
	/// The PNG as the browser encoded it.
	pub png: Vec<u8>,
	/// When it was taken.
	pub captured_at: Timestamp,
	/// The page it was taken on.
	pub page_url: String,
}

/// How much of a target's review list a scan is known to have walked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Coverage {
	/// Every review currently listed was seen (`gbp`, or a `maps` feed scrolled to its end).
	Complete,
	/// The feed was walked newest-first down to this estimated publication time.
	/// `None`: nothing seen carried a usable date, so nothing can be concluded.
	DownTo(Option<Timestamp>),
}

/// What one pass of a source over a target saw.
#[derive(Clone, Debug)]
pub struct Scan {
	/// Newest first where the source has an order.
	pub reviews: Vec<Observed>,
	/// How much of the list `reviews` is known to span; decides what can be judged gone.
	pub coverage: Coverage,
	/// Things that went wrong without failing the scan: missed captures, a walk cut short.
	/// Any makes the run `partial`.
	pub warnings: Vec<String>,
	/// A walk stopped short of the archived part of the list (by its limit, or by the page
	/// failing under it): the last card it read. The next scan reads on past it.
	pub cut_after: Option<String>,
}

/// A review the archive already holds, as much of it as reconciling needs.
#[derive(Clone, Debug, PartialEq)]
pub struct KnownReview {
	/// Its id in the archive.
	pub id: ReviewId,
	/// The hash of its current content; see [`content_hash`].
	pub content_hash: String,
	/// No screenshot of it yet; the next scan that sees it takes one.
	pub capture_pending: bool,
	/// Not listed on the last scan that looked where it was.
	pub gone: bool,
	/// The estimate made when it was first seen.
	pub published_est: Option<Timestamp>,
	/// The date as the source printed it when `published_est` was made; says how precise
	/// that estimate is.
	pub published_raw: Option<String>,
	/// What a capture of it on the public page is matched against (`gbp`).
	pub author: String,
	/// See `author`.
	pub rating: Option<u8>,
	/// See `author`.
	pub text: Option<String>,
}

impl KnownReview {
	/// The earliest it can have been published; `None` when that is unknowable.
	pub fn earliest(&self) -> Option<Timestamp> {
		relative_date::lower_bound(self.published_raw.as_deref()?, self.published_est?)
	}
}

/// What the archive already holds for a target, keyed by `source_review_id`.
#[derive(Clone, Debug, Default)]
pub struct Known {
	/// Keyed by the source's id for the review.
	pub reviews: HashMap<String, KnownReview>,
	/// No scan of the target has succeeded yet (ad-hoc captures do not count): its walk
	/// reads the whole list, down to the first scan's limit, whatever is archived already.
	pub initial: bool,
	/// Where the last walk was cut short ([`Scan::cut_after`]) with the archive not caught
	/// up below it since: the next walk must get past this review before a run of archived
	/// cards may end it.
	pub cut_after: Option<String>,
}

impl Known {
	/// Whether the archive holds this review.
	pub fn contains(&self, source_review_id: &str) -> bool {
		self.reviews.contains_key(source_review_id)
	}

	/// Whether a card with this id should be screenshotted on this pass.
	pub fn wants_capture(&self, source_review_id: &str) -> bool {
		self.reviews.get(source_review_id).is_none_or(|k| k.capture_pending)
	}
}

/// Identity of a review's content: whatever makes a new version when it changes.
pub fn content_hash(rating: Option<u8>, text: Option<&str>, reply: Option<&str>) -> String {
	let mut h = Sha256::new();
	h.update(rating.map_or(String::new(), |r| r.to_string()));
	h.update([0]);
	h.update(normalise_ws(text.unwrap_or_default()));
	h.update([0]);
	h.update(normalise_ws(reply.unwrap_or_default()));
	hex(&h.finalize())
}

/// Collapses runs of whitespace, so a re-render that only reflows text is not an edit.
pub fn normalise_ws(s: &str) -> String {
	s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Lowercase hex of the bytes.
pub fn hex(bytes: &[u8]) -> String {
	use fmt::Write;
	bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
		let _ = write!(s, "{b:02x}"); // writing into a String cannot fail
		s
	})
}

/// Parses `3600` (seconds), `90m`, `6h`, `1d`, `1w`.
pub fn parse_interval(s: &str) -> Result<Duration, Rejected> {
	let s = s.trim();
	let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
	let (n, unit) = s.split_at(split);
	let n: u64 = n
		.parse()
		.map_err(|_| Rejected::invalid(format!("interval {s:?}: expected a number followed by s, m, h, d or w")))?;
	let unit_secs = match unit.trim() {
		"" | "s" => 1,
		"m" => 60,
		"h" => 3600,
		"d" => 86_400,
		"w" => 7 * 86_400,
		other => return Err(Rejected::invalid(format!("interval {s:?}: unknown unit {other:?}, expected s, m, h, d or w"))),
	};
	// stored as SQLite's signed 64-bit integer
	n.checked_mul(unit_secs)
		.filter(|&secs| i64::try_from(secs).is_ok())
		.map(Duration::from_secs)
		.ok_or_else(|| Rejected::invalid(format!("interval {s:?} is too long")))
}

/// A date (`2026-09-01`, from its start in UTC) or a full RFC 3339 timestamp.
pub fn parse_since(s: &str) -> Result<Timestamp, Rejected> {
	if let Ok(t) = s.parse::<Timestamp>() {
		return Ok(t);
	}
	s.parse::<jiff::civil::Date>()
		.ok()
		.and_then(|d| d.to_zoned(jiff::tz::TimeZone::UTC).ok())
		.map(|z| z.timestamp())
		.ok_or_else(|| Rejected::invalid(format!("since: expected YYYY-MM-DD or an RFC 3339 timestamp, got {s:?}")))
}

/// A day, `YYYY-MM-DD`.
pub fn parse_date(s: &str) -> Result<jiff::civil::Date, Rejected> {
	s.parse().map_err(|_| Rejected::invalid(format!("expected a date as YYYY-MM-DD, got {s:?}")))
}

/// A Maps UI language: a tag like `fr` or `pt-BR`. It becomes part of the page's URL, so
/// nothing else is let through.
pub fn check_lang(lang: &str) -> Result<(), Rejected> {
	let mut parts = lang.split('-');
	let primary = parts.next().unwrap_or_default();
	let subtags: Vec<&str> = parts.collect();
	let ok = (2..=3).contains(&primary.len())
		&& primary.bytes().all(|b| b.is_ascii_lowercase())
		&& subtags.len() <= 2
		&& subtags.iter().all(|t| (2..=8).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric()));
	if ok {
		Ok(())
	} else {
		Err(Rejected::invalid(format!("lang {lang:?}: expected a language tag like fr or pt-BR")))
	}
}

/// Timestamps as the archive stores and serves them: `YYYY-MM-DDTHH:MM:SSZ`, UTC, whole
/// seconds. One shape, so that they sort as they read.
pub fn fmt_ts(t: Timestamp) -> String {
	t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn content_hash_ignores_reflow_but_not_edits() {
		let a = content_hash(Some(5), Some("Great  coffee\n"), None);
		assert_eq!(a, content_hash(Some(5), Some("Great coffee"), None));
		assert_ne!(a, content_hash(Some(4), Some("Great coffee"), None));
		assert_ne!(a, content_hash(Some(5), Some("Great coffee"), Some("Thanks!")));
		// a reply that is empty and a reply that is missing are the same content
		assert_eq!(content_hash(Some(5), None, None), content_hash(Some(5), Some(""), Some("")));
	}

	#[test]
	fn gbp_location_forms() {
		let want = GbpLocation {
			account: "123".into(),
			location: "456".into(),
		};
		assert_eq!("123/456".parse::<GbpLocation>().unwrap(), want);
		assert_eq!("accounts/123/locations/456".parse::<GbpLocation>().unwrap(), want);
		assert!("123".parse::<GbpLocation>().is_err());
		assert!("123/".parse::<GbpLocation>().is_err());
		assert!("123/456?x=1".parse::<GbpLocation>().is_err());
		assert!("../456".parse::<GbpLocation>().is_err());
	}

	#[test]
	fn langs() {
		for ok in ["fr", "en", "haw", "pt-BR", "zh-Hant-TW"] {
			assert!(check_lang(ok).is_ok(), "{ok}");
		}
		for bad in ["", "f", "FR", "fr&q=x", "fr-", "fr-a", "en-US-x-y", "fr BR"] {
			assert!(check_lang(bad).is_err(), "{bad}");
		}
	}

	#[test]
	fn intervals() {
		assert_eq!(parse_interval("6h").unwrap(), Duration::from_secs(6 * 3600));
		assert_eq!(parse_interval("90m").unwrap(), Duration::from_secs(90 * 60));
		assert_eq!(parse_interval("3600").unwrap(), Duration::from_secs(3600));
		assert_eq!(parse_interval("1d").unwrap(), Duration::from_secs(86_400));
		assert!(parse_interval("soon").is_err());
	}
}
