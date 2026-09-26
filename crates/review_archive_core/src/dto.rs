//! The JSON the archive speaks: what the HTTP API returns and the CLI prints. Shared by
//! the server and the client, so the two cannot drift apart.
//!
//! Timestamps are strings in [`fmt_ts`](crate::fmt_ts)'s shape.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Target, TargetKind, fmt_ts};

/// A watched place.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
	/// Where the API serves that capture: `/captures/<sha256>.png`.
	pub capture_url: Option<String>,
}

/// Where the API serves a capture.
pub fn capture_url(sha256: &str) -> String {
	format!("/captures/{sha256}.png")
}

/// One target's activity on one UTC day.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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

/// `POST /targets`: watch a place. One of `place` and `maps_url`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NewTarget {
	/// A place id (or a Maps URL; either field takes both).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub place: Option<String>,
	/// A Google Maps URL; resolved with the Places API when it carries no place id.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub maps_url: Option<String>,
	/// The place's name when unset.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub label: Option<String>,
	/// UI language of the Maps page, e.g. `fr`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub lang: Option<String>,
	/// `6h`, `1d`, or seconds; at least an hour.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub interval: Option<String>,
	/// `<account>/<location>`: read reviews through the Business Profile API.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub gbp: Option<String>,
}

/// `PATCH /targets/{id}`: what to change; absent fields stay.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TargetPatch {
	/// New label.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub label: Option<String>,
	/// New Maps UI language.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub lang: Option<String>,
	/// New interval: `6h`, `1d`, or seconds.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub interval: Option<String>,
	/// Enable or disable scanning.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub enabled: Option<bool>,
}

/// A target with how it is doing.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TargetDetail {
	/// The target.
	#[serde(flatten)]
	pub target: TargetDto,
	/// Its latest finished run.
	pub last_run: Option<RunDto>,
	/// Reviews archived.
	pub reviews: i64,
	/// Of which currently gone.
	pub gone: i64,
	/// Screenshots archived.
	pub captures: i64,
	/// When the scheduler scans it next; `null` for a disabled target.
	pub next_scan_at: Option<String>,
}

/// A run of a scan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RunDto {
	/// Its id.
	pub id: i64,
	/// The target scanned.
	pub target_id: i64,
	/// When it began.
	pub started_at: String,
	/// When it ended; `null` while running, or when the process died under it.
	pub finished_at: Option<String>,
	/// `null` until it ends.
	pub status: Option<RunStatus>,
	/// The failure, or the warnings of a partial run.
	pub error: Option<String>,
	/// What it changed.
	#[serde(flatten)]
	pub counts: Counts,
}

/// What a job does.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
	/// A registered target's scan, now.
	Scan,
	/// An ad-hoc capture of a place.
	Capture,
}

impl JobKind {
	/// As stored.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Scan => "scan",
			Self::Capture => "capture",
		}
	}
}

/// Where a job is.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
	/// Waiting for the browser.
	Queued,
	/// In the browser now.
	Running,
	/// Finished; `reviews` holds what it saw.
	Done,
	/// See `error`.
	Failed,
}

impl JobStatus {
	/// As stored.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Queued => "queued",
			Self::Running => "running",
			Self::Done => "done",
			Self::Failed => "failed",
		}
	}

	/// Done or failed: it will not change again.
	pub fn is_finished(self) -> bool {
		matches!(self, Self::Done | Self::Failed)
	}
}

/// `202`: the job was queued.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JobAccepted {
	/// Poll `GET /jobs/{job_id}`.
	pub job_id: i64,
}

/// A job and, once done, what it saw.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JobDto {
	/// Its id.
	pub id: i64,
	/// What it does.
	pub kind: JobKind,
	/// Where it is.
	pub status: JobStatus,
	/// The target it scans (for a capture, the implicit one its results are kept under).
	pub target_id: i64,
	/// When it was queued.
	pub created_at: String,
	/// When the browser took it.
	pub started_at: Option<String>,
	/// When it ended.
	pub finished_at: Option<String>,
	/// Why it failed.
	pub error: Option<String>,
	/// The run it made.
	pub run: Option<RunDto>,
	/// On `done`: the reviews the run listed, newest first, with their capture URLs.
	pub reviews: Option<Vec<ReviewDto>>,
}

/// `POST /captures`: capture a place without registering it. One of `place` and `maps_url`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CaptureRequest {
	/// A place id (or a Maps URL).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub place: Option<String>,
	/// A Google Maps URL.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub maps_url: Option<String>,
	/// UI language of the Maps page.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub lang: Option<String>,
	/// Cards read at most; the per-scan default when unset.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub max_reviews: Option<usize>,
	/// Only these Google review ids; the walk stops once all are found.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub review_ids: Option<Vec<String>>,
}

/// One version of a review's content.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct VersionDto {
	/// When this content was first seen.
	pub seen_at: String,
	/// Its hash.
	pub content_hash: String,
	/// Stars.
	pub rating: Option<i64>,
	/// Text.
	pub text: Option<String>,
	/// Owner's reply.
	pub reply: Option<String>,
}

/// A stored screenshot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CaptureDto {
	/// Its hash, and its name in the blob store.
	pub sha256: String,
	/// When it was taken.
	pub captured_at: String,
	/// Pixels.
	pub width: i64,
	/// Pixels.
	pub height: i64,
	/// The page it was taken on.
	pub page_url: String,
	/// What took it.
	pub scanner_version: String,
	/// `/captures/<sha256>.png`.
	pub url: String,
}

/// A review with its full history.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReviewDetail {
	/// The review as last seen.
	#[serde(flatten)]
	pub review: ReviewDto,
	/// Every distinct content, oldest first.
	pub versions: Vec<VersionDto>,
	/// Every screenshot, oldest first.
	pub captures: Vec<CaptureDto>,
}

/// What a webhook can subscribe to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum Event {
	/// A review archived for the first time.
	#[serde(rename = "review.new")]
	ReviewNew,
	/// A new version of a review.
	#[serde(rename = "review.changed")]
	ReviewChanged,
	/// A review no longer listed where a scan looked.
	#[serde(rename = "review.gone")]
	ReviewGone,
	/// A gone review listed again.
	#[serde(rename = "review.reappeared")]
	ReviewReappeared,
	/// A scan failed.
	#[serde(rename = "run.failed")]
	RunFailed,
}

impl Event {
	/// Every event.
	pub const ALL: [Self; 5] = [Self::ReviewNew, Self::ReviewChanged, Self::ReviewGone, Self::ReviewReappeared, Self::RunFailed];

	/// `review.new`, …
	pub fn as_str(self) -> &'static str {
		match self {
			Self::ReviewNew => "review.new",
			Self::ReviewChanged => "review.changed",
			Self::ReviewGone => "review.gone",
			Self::ReviewReappeared => "review.reappeared",
			Self::RunFailed => "run.failed",
		}
	}
}

/// `POST /webhooks`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NewWebhook {
	/// Where deliveries are POSTed; `http` or `https`.
	pub url: String,
	/// What to deliver.
	pub events: Vec<Event>,
	/// The HMAC-SHA256 key deliveries are signed with (`X-Signature: sha256=<hex>`); 16+
	/// characters. Never returned.
	pub secret: String,
}

/// A webhook, without its secret.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WebhookDto {
	/// Its id.
	pub id: i64,
	/// Where deliveries go.
	pub url: String,
	/// What it gets.
	pub events: Vec<Event>,
	/// When it was added.
	pub created_at: String,
}

/// The body of a webhook delivery.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EventPayload {
	/// What happened.
	pub event: Event,
	/// When the archive recorded it.
	pub occurred_at: String,
	/// The target.
	pub target_id: i64,
	/// The review, for `review.*`.
	pub review: Option<ReviewDto>,
	/// The run, for `run.failed`.
	pub run: Option<RunDto>,
}

/// An error response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ErrorBody {
	/// What went wrong; `internal error` for a 5xx, whose details stay in the server log.
	pub error: String,
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
