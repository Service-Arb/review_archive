//! The JSON the archive speaks: what the HTTP API returns and the CLI prints. Shared by
//! the server and the client, so the two cannot drift apart.
//!
//! Timestamps are strings in [`fmt_ts`](crate::fmt_ts)'s shape.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Target, TargetKind, fmt_ts};

/// A watched place.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TargetDto {
	/// Its id in the archive.
	pub id: i64,
	/// What people call it.
	pub label: String,
	/// `maps` or `gbp`.
	pub kind: TargetKind,
	/// The Google place id.
	pub place_id: String,
	/// Set for `gbp` targets.
	pub gbp_account: Option<String>,
	/// Set for `gbp` targets.
	pub gbp_location: Option<String>,
	/// UI language of the Maps page.
	pub lang: String,
	/// Seconds between scans, before jitter.
	pub interval_secs: u64,
	/// Disabled targets keep their archive but are not scanned.
	pub enabled: bool,
	/// When it was added.
	pub created_at: String,
}

impl From<Target> for TargetDto {
	fn from(t: Target) -> Self {
		Self {
			id: t.id.0,
			label: t.label,
			kind: t.kind,
			place_id: t.place_id,
			gbp_account: t.gbp.as_ref().map(|g| g.account.clone()),
			gbp_location: t.gbp.map(|g| g.location),
			lang: t.lang,
			interval_secs: t.interval.as_secs(),
			enabled: t.enabled,
			created_at: fmt_ts(t.created_at),
		}
	}
}

/// A review as archived.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReviewDto {
	/// Its id in the archive.
	pub id: i64,
	/// The target it belongs to.
	pub target_id: i64,
	/// The source's id for it.
	pub source_review_id: String,
	/// The reviewer's display name.
	pub author: String,
	/// The reviewer's public profile.
	pub author_url: Option<String>,
	/// 1–5 stars, as last seen.
	pub rating: Option<i64>,
	/// The text, as last seen.
	pub text: Option<String>,
	/// The owner's response, as last seen.
	pub reply: Option<String>,
	/// Photos attached.
	pub photo_count: i64,
	/// The date as the source printed it.
	pub published_raw: Option<String>,
	/// When it was published, estimated when first seen.
	pub published_est: Option<String>,
	/// When the archive first saw it.
	pub first_seen: String,
	/// When a scan last listed it.
	pub last_seen: String,
	/// Set while a scan that looked where it was did not list it.
	pub gone_at: Option<String>,
	/// No screenshot of it yet.
	pub capture_pending: bool,
	/// The first capture's hash; the one that shows the review as it first appeared.
	pub capture_sha256: Option<String>,
	/// When that capture was taken.
	pub captured_at: Option<String>,
}

/// One target's activity on one UTC day.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DayStats {
	/// The target.
	pub target_id: i64,
	/// `YYYY-MM-DD`.
	pub day: String,
	/// Reviews first seen that day.
	pub new: i64,
	/// Edits seen that day.
	pub changed: i64,
	/// Reviews that went missing that day.
	pub gone: i64,
	/// Mean rating, as first seen, of the reviews first seen that day.
	pub mean_rating: Option<f64>,
	/// Count of reviews first seen that day per star, index 0 = one star.
	pub histogram: [i64; 5],
}

/// How a scan went.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
	/// Everything it tried worked.
	Ok,
	/// Stored what it saw, with warnings (a missed screenshot, a walk cut short).
	Partial,
	/// Nothing stored; see the error.
	Failed,
}

impl RunStatus {
	/// `"ok"`, `"partial"`, `"failed"`.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Ok => "ok",
			Self::Partial => "partial",
			Self::Failed => "failed",
		}
	}
}

/// What one scan did to the archive.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Counts {
	/// Distinct reviews listed.
	pub seen: u32,
	/// Archived for the first time.
	pub new: u32,
	/// A new version recorded.
	pub changed: u32,
	/// Newly marked gone.
	pub gone: u32,
}

/// One scan of one target, as reported.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunSummary {
	/// The target.
	pub target: i64,
	/// Its label.
	pub label: String,
	/// How it went.
	pub status: RunStatus,
	/// What it changed.
	#[serde(flatten)]
	pub counts: Counts,
	/// Screenshots stored.
	pub captured: u32,
	/// Whether it saw the whole list.
	pub complete: bool,
	/// The failure, or the warnings of a partial run.
	pub error: Option<String>,
}

impl fmt::Display for RunSummary {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let status = match self.status {
			RunStatus::Ok => "ok",
			RunStatus::Partial => "partial",
			RunStatus::Failed => "FAILED",
		};
		write!(
			f,
			"#{} {:<24} {status:<8} seen {:>4}  new {:>4}  changed {:>3}  gone {:>3}  captured {:>4}{}",
			self.target,
			self.label,
			self.counts.seen,
			self.counts.new,
			self.counts.changed,
			self.counts.gone,
			self.captured,
			if self.complete { "  (whole list)" } else { "" }
		)?;
		if let Some(e) = &self.error {
			write!(f, "\n    {e}")?;
		}
		Ok(())
	}
}

/// Day stats as CSV, one row per target and day, the histogram as `stars_1` … `stars_5`.
pub fn stats_csv(rows: &[DayStats]) -> eyre::Result<String> {
	let mut w = csv::Writer::from_writer(Vec::new());
	w.write_record(["target_id", "day", "new", "changed", "gone", "mean_rating", "stars_1", "stars_2", "stars_3", "stars_4", "stars_5"])?;
	for r in rows {
		let mut rec = vec![
			r.target_id.to_string(),
			r.day.clone(),
			r.new.to_string(),
			r.changed.to_string(),
			r.gone.to_string(),
			r.mean_rating.map(|m| format!("{m:.3}")).unwrap_or_default(),
		];
		rec.extend(r.histogram.iter().map(ToString::to_string));
		w.write_record(&rec)?;
	}
	Ok(String::from_utf8(w.into_inner()?)?)
}
