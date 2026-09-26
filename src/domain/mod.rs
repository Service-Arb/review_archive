//! What a review archive is, with no browser, database or clock in it.
//!
//! Sources, storage and the scheduler loop are adapters around this: they hand
//! it observations and the current time, and apply what it decides.

pub mod reconcile;
pub mod relative_date;
pub mod schedule;

use std::{collections::HashMap, fmt, str::FromStr, time::Duration};

use jiff::Timestamp;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TargetId(pub i64);

impl fmt::Display for TargetId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.fmt(f)
	}
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReviewId(pub i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetKind {
	Maps,
	Gbp,
}

impl TargetKind {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Maps => "maps",
			Self::Gbp => "gbp",
		}
	}
}

impl FromStr for TargetKind {
	type Err = eyre::Report;

	fn from_str(s: &str) -> eyre::Result<Self> {
		match s {
			"maps" => Ok(Self::Maps),
			"gbp" => Ok(Self::Gbp),
			other => Err(eyre::eyre!("unknown target kind {other:?}, expected maps or gbp")),
		}
	}
}

/// The `accounts/{account}/locations/{location}` pair of a Business Profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GbpLocation {
	pub account: String,
	pub location: String,
}

impl FromStr for GbpLocation {
	type Err = eyre::Report;

	fn from_str(s: &str) -> eyre::Result<Self> {
		let s = s.trim().trim_start_matches("accounts/");
		let (account, location) = s.split_once('/').ok_or_else(|| eyre::eyre!("expected <account>/<location>, got {s:?}"))?;
		let location = location.trim_start_matches("locations/");
		eyre::ensure!(!account.is_empty() && !location.is_empty() && !location.contains('/'), "expected <account>/<location>, got {s:?}");
		Ok(Self {
			account: account.to_owned(),
			location: location.to_owned(),
		})
	}
}

#[derive(Clone, Debug)]
pub struct Target {
	pub id: TargetId,
	pub label: String,
	pub kind: TargetKind,
	pub place_id: String,
	/// Set exactly when `kind` is `Gbp`.
	pub gbp: Option<GbpLocation>,
	pub lang: String,
	pub interval: Duration,
	pub enabled: bool,
	pub created_at: Timestamp,
}

/// One review as a source saw it on this scan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observed {
	pub source_review_id: String,
	pub author: String,
	pub author_url: Option<String>,
	pub rating: Option<u8>,
	pub text: Option<String>,
	pub reply: Option<String>,
	pub photo_count: u32,
	pub published_raw: Option<String>,
	pub published_est: Option<Timestamp>,
	pub capture: Option<Capture>,
}

impl Observed {
	pub fn content_hash(&self) -> String {
		content_hash(self.rating, self.text.as_deref(), self.reply.as_deref())
	}
}

/// A PNG of the review card, straight from the browser.
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
	pub png: Vec<u8>,
	pub captured_at: Timestamp,
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

#[derive(Clone, Debug)]
pub struct Scan {
	/// Newest first where the source has an order.
	pub reviews: Vec<Observed>,
	pub coverage: Coverage,
	/// Things that went wrong without failing the scan: missed captures, a walk cut short.
	/// Any makes the run `partial`.
	pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct KnownReview {
	pub id: ReviewId,
	pub content_hash: String,
	pub capture_pending: bool,
	pub gone: bool,
	pub published_est: Option<Timestamp>,
	/// What a capture of it on the public page is matched against (`gbp`).
	pub author: String,
	pub rating: Option<u8>,
	pub text: Option<String>,
}

/// What the archive already holds for a target, keyed by `source_review_id`.
#[derive(Clone, Debug, Default)]
pub struct Known {
	pub reviews: HashMap<String, KnownReview>,
}

impl Known {
	pub fn is_empty(&self) -> bool {
		self.reviews.is_empty()
	}

	pub fn contains(&self, source_review_id: &str) -> bool {
		self.reviews.contains_key(source_review_id)
	}

	/// Whether a card with this id should be screenshotted on this pass.
	pub fn wants_capture(&self, source_review_id: &str) -> bool {
		self.reviews.get(source_review_id).is_none_or(|k| k.capture_pending)
	}
}

/// A source of reviews for a target. Read-only by contract: no implementation
/// may post, edit, reply to or report anything.
#[allow(async_fn_in_trait)] // only ever used through generics inside this crate, never as `dyn` or across a Send bound it would hide
pub trait ReviewSource {
	async fn scan(&self, target: &Target, known: &Known) -> eyre::Result<Scan>;
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

pub fn hex(bytes: &[u8]) -> String {
	use fmt::Write;
	bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
		let _ = write!(s, "{b:02x}"); // writing into a String cannot fail
		s
	})
}

/// Parses `3600` (seconds), `90m`, `6h`, `1d`, `1w`.
pub fn parse_interval(s: &str) -> eyre::Result<Duration> {
	let s = s.trim();
	let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
	let (n, unit) = s.split_at(split);
	let n: u64 = n.parse().map_err(|_| eyre::eyre!("interval {s:?}: expected a number followed by s, m, h, d or w"))?;
	let unit_secs = match unit.trim() {
		"" | "s" => 1,
		"m" => 60,
		"h" => 3600,
		"d" => 86_400,
		"w" => 7 * 86_400,
		other => eyre::bail!("interval {s:?}: unknown unit {other:?}, expected s, m, h, d or w"),
	};
	Ok(Duration::from_secs(n * unit_secs))
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
