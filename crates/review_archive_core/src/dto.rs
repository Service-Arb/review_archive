//! The JSON the archive speaks: what the HTTP API returns and the CLI prints. Shared by
//! the server and the client, so the two cannot drift apart.
//!
//! Timestamps are strings in [`crate::fmt_ts`]'s shape.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

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
		let (gbp_account, gbp_location) = t.gbp.map(|g| (g.account, g.location)).unzip();
		Self {
			id: t.id.0,
			label: t.label,
			kind: t.kind,
			place_id: t.place_id,
			gbp_account,
			gbp_location,
			lang: t.lang,
			interval_secs: t.interval.duration().as_secs(),
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
	/// The date as the source printed it when `published_est` was made from it.
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
	/// Where the API serves that capture: `/captures/<sha256>.avif`.
	pub capture_url: Option<String>,
}

/// Where the API serves a capture.
pub fn capture_url(sha256: &str) -> String {
	format!("/captures/{sha256}.avif")
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
	/// Reviews that went missing that day, whether or not they came back later.
	pub gone: i64,
	/// Mean rating, as first seen, of the reviews first seen that day.
	pub mean_rating: Option<f64>,
	/// Count of reviews first seen that day per star, index 0 = one star.
	pub histogram: [i64; 5],
}

/// How a scan went. Stored as its lowercase name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::AsRefStr, strum::EnumString)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum RunStatus {
	/// Everything it tried worked.
	Ok,
	/// Stored what it saw, with warnings (a missed screenshot, a walk cut short).
	Partial,
	/// Nothing stored; see the error.
	Failed,
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
		let status = if self.status == RunStatus::Failed { "FAILED" } else { self.status.as_ref() };
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
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default)]
pub struct NewTarget {
	/// A place id (or a Maps URL; either field takes both).
	pub place: Option<String>,
	/// A Google Maps URL; resolved with the Places API when it carries no place id.
	pub maps_url: Option<String>,
	/// The place's name when unset.
	pub label: Option<String>,
	/// UI language of the Maps page, e.g. `fr`.
	pub lang: Option<String>,
	/// `6h`, `1d`, or seconds; at least an hour.
	pub interval: Option<String>,
	/// `<account>/<location>`: read reviews through the Business Profile API.
	pub gbp: Option<String>,
}

/// `PATCH /targets/{id}`: what to change; absent fields stay.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default)]
pub struct TargetPatch {
	/// New label.
	pub label: Option<String>,
	/// New Maps UI language.
	pub lang: Option<String>,
	/// New interval: `6h`, `1d`, or seconds.
	pub interval: Option<String>,
	/// Enable or disable scanning.
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
	/// When it ended; `null` while running.
	pub finished_at: Option<String>,
	/// `null` until it ends.
	pub status: Option<RunStatus>,
	/// The failure, or the warnings of a partial run.
	pub error: Option<String>,
	/// What it changed.
	#[serde(flatten)]
	pub counts: Counts,
	/// What its walk cost, paid or not.
	pub tokens: i64,
}

/// What a job does. Stored as its lowercase name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::AsRefStr, strum::Display, strum::EnumString)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum JobKind {
	/// A registered target's scan, now.
	Scan,
	/// An ad-hoc capture of a place.
	Capture,
}

/// Where a job is. Stored as its lowercase name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::AsRefStr, strum::Display, strum::EnumString)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
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
	/// On `done`: the reviews the run listed, newest first, with their capture URLs — only
	/// the requested ones, for a capture of named reviews.
	pub reviews: Option<Vec<ReviewDto>>,
}

/// What an ad-hoc capture reads.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default)]
pub struct CaptureLimits {
	/// Cards read at most; the per-scan default when unset, `defaults.max_reviews_initial`
	/// at most.
	pub max_reviews: Option<usize>,
	/// Only these Google review ids; the walk stops once all are found.
	pub review_ids: Option<Vec<String>>,
}

/// `POST /captures`: capture a place without registering it. One of `place` and `maps_url`.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default)]
pub struct CaptureRequest {
	/// A place id (or a Maps URL).
	pub place: Option<String>,
	/// A Google Maps URL.
	pub maps_url: Option<String>,
	/// UI language of the Maps page.
	pub lang: Option<String>,
	/// How much to read.
	#[serde(flatten)]
	pub limits: CaptureLimits,
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
	/// `/captures/<sha256>.avif`.
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

/// What a webhook can subscribe to. Stored and sent (`X-Event`) under its dotted name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, strum::AsRefStr, strum::EnumString)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum Event {
	/// A review archived for the first time.
	#[serde(rename = "review.new")]
	#[strum(serialize = "review.new")]
	ReviewNew,
	/// A new version of a review.
	#[serde(rename = "review.changed")]
	#[strum(serialize = "review.changed")]
	ReviewChanged,
	/// A review no longer listed where a scan looked.
	#[serde(rename = "review.gone")]
	#[strum(serialize = "review.gone")]
	ReviewGone,
	/// A gone review listed again.
	#[serde(rename = "review.reappeared")]
	#[strum(serialize = "review.reappeared")]
	ReviewReappeared,
	/// A scan failed.
	#[serde(rename = "run.failed")]
	#[strum(serialize = "run.failed")]
	RunFailed,
}

/// `POST /webhooks`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NewWebhook {
	/// Where deliveries are POSTed; `http` or `https`, to a host the archive allows.
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

/// `GET /targets/{id}/reviews`.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams), into_params(parameter_in = Query))]
#[serde(default)]
pub struct ReviewsQuery {
	/// `YYYY-MM-DD` or RFC 3339: first seen at or after.
	pub since: Option<String>,
	/// Only gone (`true`) or only listed (`false`) reviews.
	pub gone: Option<bool>,
}

/// `GET /targets/{id}/runs`.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams), into_params(parameter_in = Query))]
#[serde(default)]
pub struct RunsQuery {
	/// At most this many, newest first (default 20, at most 500).
	pub limit: Option<u32>,
}

/// `GET /targets/{id}/export.zip`.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams), into_params(parameter_in = Query))]
#[serde(default)]
pub struct ExportQuery {
	/// `YYYY-MM-DD` or RFC 3339: reviews first seen at or after.
	pub since: Option<String>,
}

/// `POST /captures`.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams), into_params(parameter_in = Query))]
#[serde(default)]
pub struct WaitQuery {
	/// Hold the request up to this many seconds (at most 120) and answer with the finished
	/// job if it finishes by then.
	pub wait: Option<u64>,
}

/// `GET /stats`.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams), into_params(parameter_in = Query))]
#[serde(default)]
pub struct StatsQuery {
	/// One target; every target when unset.
	pub target: Option<i64>,
	/// `YYYY-MM-DD`, inclusive.
	pub from: Option<String>,
	/// `YYYY-MM-DD`, inclusive.
	pub to: Option<String>,
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

/// A managing gmail: the Google manager account a member's places are attached to.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GmailDto {
	/// Its id.
	pub id: i64,
	/// The address.
	pub gmail: String,
	/// Off: its places are not scanned on its account.
	pub enabled: bool,
	/// When the member added it.
	pub created_at: String,
}

/// `PATCH /me/gmails/{gmail}` and `PATCH /me/gmails/{gmail}/tracks/{target}`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Switch {
	/// On or off.
	pub enabled: bool,
}

/// `POST /me/gmails`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NewGmail {
	/// The manager account's address, or an alias for it.
	pub gmail: String,
}

/// A tracked place, as the member's overview shows it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LocationSummary {
	/// The place.
	pub target: TargetDto,
	/// This gmail's track of it is on; it is scanned while any member's is, under a gmail that is on.
	pub enabled: bool,
	/// Reviews posted over the last 7 days, as their dates are estimated; gone ones too.
	pub new_7d: i64,
	/// Owner's posts published over the last 7 days, as their dates are estimated.
	pub posts_7d: i64,
	/// The owner's post seen most recently first.
	pub latest_post: Option<PostDto>,
	/// Screenshots taken over the last 7 days.
	pub snapshots_7d: i64,
	/// Screenshots taken over the last 30 days.
	pub snapshots_30d: i64,
	/// Reviews listed now.
	pub live: i64,
	/// Of which the owner has replied to.
	pub responded: i64,
	/// How many reviews Google says the place has, as of the last scan that read it.
	pub listed: Option<i64>,
	/// Gone, with no open appeal from this gmail.
	pub removed: i64,
	/// Gone, with an open appeal from this gmail.
	pub reinstating: i64,
	/// When its latest finished run ended.
	pub last_run_at: Option<String>,
	/// How it went.
	pub last_run_status: Option<RunStatus>,
	/// Not scanned: every member tracking it is out of tokens.
	pub held: bool,
}

/// A post by a place's owner.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PostDto {
	/// Line breaks kept.
	pub text: String,
	/// When it was posted, estimated when first seen; `null` when the page printed a date.
	pub published_est: Option<String>,
	/// When the archive first saw it.
	pub first_seen: String,
}

/// A gmail and its places, by snapshots over 7 days, most first.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GmailOverview {
	/// The gmail.
	#[serde(flatten)]
	pub gmail: GmailDto,
	/// What it tracks.
	pub locations: Vec<LocationSummary>,
}

/// An appeal of a removed review.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReinstatementDto {
	/// When it was requested.
	pub requested_at: String,
	/// When a scan listed the review again, if one has.
	pub reinstated_at: Option<String>,
}

/// A review on a board, with this gmail's latest appeal of it that was not withdrawn.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BoardCard {
	/// The review.
	pub review: ReviewDto,
	/// The appeal.
	pub reinstatement: Option<ReinstatementDto>,
}

/// One tracked place's reviews in three columns.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Board {
	/// Listed now, newest first sighting first; a reinstated one carries its appeal.
	pub snapshotted: Vec<BoardCard>,
	/// Gone without an open appeal, most recently gone first.
	pub removed: Vec<BoardCard>,
	/// Gone with an open appeal, most recently requested first.
	pub reinstating: Vec<BoardCard>,
}

/// `POST /me/gmails/{id}/tracks`: watch a place under this gmail. The place is watched once
/// however many members track it; one already watched with this language is reused.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default)]
pub struct NewTrack {
	/// A place id or a Google Maps URL.
	pub place: String,
	/// The place's name when unset.
	pub label: Option<String>,
	/// UI language of the Maps page, e.g. `fr`.
	pub lang: Option<String>,
	/// `<account>/<location>`: read reviews through the Business Profile API, which needs
	/// the archive's Google account among the location's managers.
	pub gbp: Option<String>,
}

/// A member's Telegram channel for events.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TgChannelDto {
	/// Its id.
	pub id: i64,
	/// As pasted: `@channel`, `-100…`, or `<group>/<topic>`.
	pub destination: String,
	/// Only this gmail's places; every place the member tracks when unset.
	pub gmail_id: Option<i64>,
	/// What it gets.
	pub events: Vec<Event>,
	/// When it was added.
	pub created_at: String,
}

/// `POST /me/tg-channels`. The archive's bot has to be in the chat and allowed to post.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NewTgChannel {
	/// `@channel`, a `-100…` chat id, or `<group>/<topic>` for a forum topic.
	pub destination: String,
	/// Only this gmail's places; every place the member tracks when unset.
	pub gmail_id: Option<i64>,
	/// What to send.
	pub events: Vec<Event>,
}

/// `GET /me`: who is signed in, whoever they act as.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Me {
	/// Their verified email.
	pub email: String,
	/// Their valeratrades.com username.
	pub username: String,
	/// May act as any member (`X-Member`).
	pub admin: bool,
	/// Their tokens.
	pub tokens: TokensDto,
}

/// A member's tokens: what their places' scans are paid with.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TokensDto {
	/// What they hold now.
	pub balance: i64,
	/// Added per day while under `cap`.
	pub daily: i64,
	/// Where daily renewal stops.
	pub cap: i64,
}

/// Why a member's balance moved. Stored as its lowercase name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::AsRefStr, strum::Display, strum::EnumString)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum TokenKind {
	/// Daily renewal.
	Accrual,
	/// Given by an admin.
	Grant,
	/// Bought; recorded by an admin.
	Purchase,
	/// An admin set the balance; the row is the difference.
	Set,
	/// A scan of their place.
	Charge,
}

/// `GET /me/tokens`: a row of the member's ledger.
#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LedgerEntry {
	/// When.
	pub at: String,
	/// How much the balance moved.
	pub delta: i64,
	/// Why.
	pub kind: TokenKind,
	/// The admin who recorded it.
	pub by: Option<String>,
	/// What they wrote with it.
	pub note: Option<String>,
	/// A charge's run.
	pub run_id: Option<i64>,
	/// A charge's place.
	pub target_id: Option<i64>,
	/// See `target_id`.
	pub target_label: Option<String>,
}

/// `POST /members/{email}/tokens`: one change of a member's balance.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TokensChange {
	/// What changes.
	#[serde(flatten)]
	pub change: BalanceChange,
	/// A payment reference, a reason.
	pub note: Option<String>,
}

/// See [`TokensChange`]: `{"set": n}`, `{"grant": n}` or `{"purchase": n}`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum BalanceChange {
	/// The balance becomes this.
	Set(i64),
	/// Adds this.
	Grant(i64),
	/// Adds this, bought.
	Purchase(i64),
}

/// `GET /members`: a member as valeratrades.com lists them.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MemberDto {
	/// The email the group lists.
	pub email: String,
	/// `None`: not signed up yet.
	pub username: Option<String>,
	/// Their Google name, if they signed in with Google.
	pub display_name: Option<String>,
	/// Their token balance.
	pub balance: i64,
}

/// On `/me` routes: the member an admin acts as.
pub const MEMBER_HEADER: &str = "x-member";
