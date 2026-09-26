//! The HTTP API: everything the CLI does, for other services. Everything but `/health`
//! and `/openapi.json` wants the bearer token. JSON in and out, DTOs from
//! `review_archive_core::dto`.

use std::{sync::Arc, time::Duration};

use axum::{
	Json, Router,
	body::Body,
	extract::{Path, Query, Request, State},
	http::{HeaderMap, StatusCode, header},
	middleware::{self, Next},
	response::{IntoResponse, Response},
	routing::{get, post},
};
use review_archive::{AddTarget, Archive, Rejected};
use review_archive_core::{
	GbpLocation, ReviewId, TargetId,
	dto::{
		CaptureDto, CaptureRequest, Counts, DayStats, ErrorBody, Event, EventPayload, JobAccepted, JobDto, JobKind, JobStatus, NewTarget, NewWebhook, ReviewDetail, ReviewDto, RunDto,
		RunStatus, TargetDetail, TargetDto, TargetPatch, VersionDto, WebhookDto, stats_csv,
	},
	parse_interval, parse_since,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use utoipa::{
	IntoParams, OpenApi,
	openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};

use crate::worker::Signals;

/// The longest `POST /captures?wait=` holds a request.
pub const MAX_WAIT: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct AppState {
	pub archive: Archive,
	/// SHA-256 of the token: compared as digests, so the comparison time says nothing about it.
	pub token_digest: Arc<[u8; 32]>,
	pub signals: Arc<Signals>,
}

impl AppState {
	pub fn new(archive: Archive, token: &str, signals: Arc<Signals>) -> Self {
		Self {
			archive,
			token_digest: Arc::new(Sha256::digest(token.as_bytes()).into()),
			signals,
		}
	}
}

pub fn router(state: AppState) -> Router {
	let authed = Router::new()
		.route("/targets", get(targets).post(add_target))
		.route("/targets/{id}", get(target).patch(patch_target).delete(delete_target))
		.route("/targets/{id}/reviews", get(reviews))
		.route("/targets/{id}/runs", get(runs))
		.route("/targets/{id}/scan", post(scan))
		.route("/targets/{id}/export.zip", get(export_zip))
		.route("/captures", post(capture))
		.route("/captures/{file}", get(capture_png))
		.route("/jobs/{id}", get(job))
		.route("/reviews/{id}", get(review))
		.route("/stats", get(stats))
		.route("/webhooks", get(webhooks).post(add_webhook))
		.route("/webhooks/{id}", axum::routing::delete(delete_webhook))
		.route_layer(middleware::from_fn_with_state(state.clone(), auth));
	Router::new()
		.route("/health", get(|| async { "ok" }))
		.route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
		.merge(authed)
		.with_state(state)
}

async fn auth(State(state): State<AppState>, headers: HeaderMap, req: Request, next: Next) -> Response {
	let given = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
	let ok = given.is_some_and(|t| {
		let d: [u8; 32] = Sha256::digest(t.as_bytes()).into();
		d.iter().zip(state.token_digest.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
	});
	if !ok {
		return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], Json(ErrorBody { error: "unauthorized".into() })).into_response();
	}
	next.run(req).await
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
	fn into_response(self) -> Response {
		(self.0, Json(ErrorBody { error: self.1 })).into_response()
	}
}

impl From<eyre::Report> for ApiError {
	/// The caller's mistakes are 404 / 400 with the reason. Anything else is a 5xx: reported,
	/// its details kept in the log rather than the response.
	fn from(e: eyre::Report) -> Self {
		match e.downcast_ref::<Rejected>() {
			Some(Rejected::NotFound(m)) => return Self(StatusCode::NOT_FOUND, m.clone()),
			Some(Rejected::Invalid(m)) => return Self(StatusCode::BAD_REQUEST, m.clone()),
			None => {}
		}
		ev_lib::error_monitoring::report(&*e);
		// warn, not error: the tracing layer would send an error as a second Sentry event
		tracing::warn!(error = %format!("{e:#}"), "request failed");
		Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
	}
}

fn bad_request(msg: impl Into<String>) -> ApiError {
	ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn not_found(msg: impl Into<String>) -> ApiError {
	ApiError(StatusCode::NOT_FOUND, msg.into())
}

type ApiResult<T> = Result<T, ApiError>;

#[utoipa::path(get, path = "/targets", tag = "targets", security(("bearer" = [])), responses((status = 200, body = [TargetDto])))]
async fn targets(State(s): State<AppState>) -> ApiResult<Json<Vec<TargetDto>>> {
	Ok(Json(s.archive.targets().await?.into_iter().map(TargetDto::from).collect()))
}

/// Watch a place. Same resolution as `target add`: a place id, or a Maps URL.
#[utoipa::path(post, path = "/targets", tag = "targets", security(("bearer" = [])), request_body = NewTarget,
	responses((status = 201, body = TargetDto), (status = 400, body = ErrorBody)))]
async fn add_target(State(s): State<AppState>, Json(req): Json<NewTarget>) -> ApiResult<(StatusCode, Json<TargetDto>)> {
	let place = req.place.or(req.maps_url).ok_or_else(|| bad_request("name the place: `place` or `maps_url`"))?;
	let interval = req.interval.as_deref().map(parse_interval).transpose().map_err(|e| bad_request(format!("interval: {e}")))?;
	let gbp = req.gbp.as_deref().map(str::parse::<GbpLocation>).transpose().map_err(|e| bad_request(format!("gbp: {e}")))?;
	let added = s
		.archive
		.add_target(AddTarget {
			place,
			label: req.label,
			lang: req.lang,
			interval,
			gbp,
			disabled: false,
		})
		.await?;
	Ok((StatusCode::CREATED, Json(added.target.into())))
}

/// A target with its last run, what is archived, and the next scheduled scan.
#[utoipa::path(get, path = "/targets/{id}", tag = "targets", security(("bearer" = [])), params(("id" = i64, Path)),
	responses((status = 200, body = TargetDetail), (status = 404, body = ErrorBody)))]
async fn target(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<TargetDetail>> {
	Ok(Json(s.archive.target_detail(TargetId(id)).await?))
}

#[utoipa::path(patch, path = "/targets/{id}", tag = "targets", security(("bearer" = [])), params(("id" = i64, Path)), request_body = TargetPatch,
	responses((status = 200, body = TargetDto), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn patch_target(State(s): State<AppState>, Path(id): Path<i64>, Json(patch): Json<TargetPatch>) -> ApiResult<Json<TargetDto>> {
	Ok(Json(s.archive.update_target(TargetId(id), &patch).await?.into()))
}

/// Disables the target. Nothing archived is deleted.
#[utoipa::path(delete, path = "/targets/{id}", tag = "targets", security(("bearer" = [])), params(("id" = i64, Path)),
	responses((status = 200, body = TargetDto), (status = 404, body = ErrorBody)))]
async fn delete_target(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<TargetDto>> {
	let patch = TargetPatch {
		enabled: Some(false),
		..Default::default()
	};
	Ok(Json(s.archive.update_target(TargetId(id), &patch).await?.into()))
}

#[derive(Deserialize, IntoParams)]
struct ReviewsQuery {
	/// `YYYY-MM-DD` or RFC 3339: first seen at or after.
	since: Option<String>,
	/// Only gone (`true`) or only listed (`false`) reviews.
	gone: Option<bool>,
}

#[utoipa::path(get, path = "/targets/{id}/reviews", tag = "reviews", security(("bearer" = [])), params(("id" = i64, Path), ReviewsQuery),
	responses((status = 200, body = [ReviewDto])))]
async fn reviews(State(s): State<AppState>, Path(id): Path<i64>, Query(q): Query<ReviewsQuery>) -> ApiResult<Json<Vec<ReviewDto>>> {
	let since = q.since.as_deref().map(parse_since).transpose().map_err(|e| bad_request(format!("since: {e}")))?;
	Ok(Json(s.archive.reviews(TargetId(id), since, q.gone).await?))
}

#[derive(Deserialize, IntoParams)]
struct RunsQuery {
	/// At most this many, newest first (default 20, at most 500).
	limit: Option<u32>,
}

#[utoipa::path(get, path = "/targets/{id}/runs", tag = "targets", security(("bearer" = [])), params(("id" = i64, Path), RunsQuery),
	responses((status = 200, body = [RunDto]), (status = 404, body = ErrorBody)))]
async fn runs(State(s): State<AppState>, Path(id): Path<i64>, Query(q): Query<RunsQuery>) -> ApiResult<Json<Vec<RunDto>>> {
	Ok(Json(s.archive.runs(TargetId(id), q.limit.unwrap_or(20).min(500)).await?))
}

/// Scan the target now, ahead of the scheduled scans.
#[utoipa::path(post, path = "/targets/{id}/scan", tag = "jobs", security(("bearer" = [])), params(("id" = i64, Path)),
	responses((status = 202, body = JobAccepted), (status = 404, body = ErrorBody)))]
async fn scan(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<(StatusCode, Json<JobAccepted>)> {
	let job_id = s.archive.enqueue_scan(TargetId(id)).await?;
	s.signals.job_queued.notify_one();
	Ok((StatusCode::ACCEPTED, Json(JobAccepted { job_id })))
}

#[derive(Deserialize, IntoParams)]
struct WaitQuery {
	/// Hold the request up to this many seconds (at most 120) and answer with the finished
	/// job if it finishes by then.
	wait: Option<u64>,
}

/// Capture a place without registering it. Its results are kept under an implicit,
/// disabled target (the place's existing one, if any), so nothing is lost.
#[utoipa::path(post, path = "/captures", tag = "jobs", security(("bearer" = [])), params(WaitQuery), request_body = CaptureRequest,
	responses((status = 202, body = JobAccepted, description = "queued, or still running when `wait` ran out"),
	          (status = 200, body = JobDto, description = "finished within `wait`"), (status = 400, body = ErrorBody)))]
async fn capture(State(s): State<AppState>, Query(q): Query<WaitQuery>, Json(req): Json<CaptureRequest>) -> ApiResult<Response> {
	// subscribed before queueing, so the job cannot end unseen in between
	let mut finished = s.signals.job_finished.subscribe();
	let job_id = s.archive.enqueue_capture(&req).await?;
	s.signals.job_queued.notify_one();
	let wait = Duration::from_secs(q.wait.unwrap_or(0)).min(MAX_WAIT);
	let deadline = tokio::time::Instant::now() + wait;
	loop {
		let job = s.archive.job(job_id).await?.ok_or_else(|| not_found(format!("no job {job_id}")))?;
		if job.status.is_finished() {
			return Ok((StatusCode::OK, Json(job)).into_response());
		}
		match tokio::time::timeout_at(deadline, finished.changed()).await {
			Ok(Ok(())) => {}
			// out of time, or the worker is gone: the job id is still good for polling
			Ok(Err(_)) | Err(_) => return Ok((StatusCode::ACCEPTED, Json(JobAccepted { job_id })).into_response()),
		}
	}
}

/// A job; on `done`, the reviews its run listed with their capture URLs.
#[utoipa::path(get, path = "/jobs/{id}", tag = "jobs", security(("bearer" = [])), params(("id" = i64, Path)),
	responses((status = 200, body = JobDto), (status = 404, body = ErrorBody)))]
async fn job(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<JobDto>> {
	Ok(Json(s.archive.job(id).await?.ok_or_else(|| not_found(format!("no job {id}")))?))
}

/// A review with every version and capture.
#[utoipa::path(get, path = "/reviews/{id}", tag = "reviews", security(("bearer" = [])), params(("id" = i64, Path)),
	responses((status = 200, body = ReviewDetail), (status = 404, body = ErrorBody)))]
async fn review(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<ReviewDetail>> {
	Ok(Json(s.archive.review(ReviewId(id)).await?.ok_or_else(|| not_found(format!("no review {id}")))?))
}

/// A capture's PNG, provenance in its `tEXt` chunks. Only what the archive recorded.
#[utoipa::path(get, path = "/captures/{sha256}.png", tag = "reviews", security(("bearer" = [])), params(("sha256" = String, Path)),
	responses((status = 200, content_type = "image/png"), (status = 404, body = ErrorBody)))]
async fn capture_png(State(s): State<AppState>, Path(file): Path<String>) -> ApiResult<Response> {
	let sha = file.strip_suffix(".png").ok_or_else(|| not_found("no such capture"))?;
	let bytes = s.archive.capture_png(sha).await?.ok_or_else(|| not_found("no such capture"))?;
	Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")], bytes).into_response())
}

#[derive(Deserialize, IntoParams)]
struct SinceQuery {
	/// `YYYY-MM-DD` or RFC 3339: reviews first seen at or after.
	since: Option<String>,
}

/// The same archive as `export`: `manifest.json` and the first capture of each review.
#[utoipa::path(get, path = "/targets/{id}/export.zip", tag = "reviews", security(("bearer" = [])), params(("id" = i64, Path), SinceQuery),
	responses((status = 200, content_type = "application/zip"), (status = 404, body = ErrorBody)))]
async fn export_zip(State(s): State<AppState>, Path(id): Path<i64>, Query(q): Query<SinceQuery>) -> ApiResult<Response> {
	let since = q.since.as_deref().map(parse_since).transpose().map_err(|e| bad_request(format!("since: {e}")))?;
	s.archive.target(TargetId(id)).await?;
	// Written to a temp file and streamed from it: an archive of thousands of PNGs does not
	// belong in memory. The file is unlinked once open; the handle keeps it readable.
	let path = std::env::temp_dir().join(format!("review_archive-export-{id}-{:016x}.zip", rand::random::<u64>()));
	let written = s.archive.export(TargetId(id), since, &path).await;
	let file = match written {
		Ok(_) => tokio::fs::File::open(&path).await.map_err(eyre::Report::new),
		Err(e) => Err(e),
	};
	if let Err(e) = tokio::fs::remove_file(&path).await
		&& e.kind() != std::io::ErrorKind::NotFound
	{
		tracing::warn!(path = %path.display(), error = %e, "removing an export's temp file");
	}
	let file = file?;
	let stream = futures::stream::unfold(file, |mut f| async move {
		use tokio::io::AsyncReadExt;
		let mut buf = vec![0u8; 64 * 1024];
		match f.read(&mut buf).await {
			Ok(0) => None,
			Ok(n) => {
				buf.truncate(n);
				Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(buf)), f))
			}
			Err(e) => Some((Err(e), f)),
		}
	});
	Ok((
		[
			(header::CONTENT_TYPE, "application/zip".to_owned()),
			(header::CONTENT_DISPOSITION, format!("attachment; filename=\"target-{id}.zip\"")),
		],
		Body::from_stream(stream),
	)
		.into_response())
}

#[derive(Deserialize, IntoParams)]
struct StatsQuery {
	target: Option<i64>,
	/// `YYYY-MM-DD`, inclusive.
	#[param(value_type = Option<String>)]
	from: Option<jiff::civil::Date>,
	/// `YYYY-MM-DD`, inclusive.
	#[param(value_type = Option<String>)]
	to: Option<jiff::civil::Date>,
}

/// Per target and UTC day: new, changed, gone, mean rating, histogram. `Accept: text/csv`
/// for CSV.
#[utoipa::path(get, path = "/stats", tag = "reviews", security(("bearer" = [])), params(StatsQuery), responses((status = 200, body = [DayStats])))]
async fn stats(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<StatsQuery>) -> ApiResult<Response> {
	let rows = s.archive.stats(q.target.map(TargetId), q.from, q.to).await?;
	let wants_csv = headers
		.get(header::ACCEPT)
		.and_then(|v| v.to_str().ok())
		.is_some_and(|a| a.split(',').any(|m| m.trim().starts_with("text/csv")));
	if wants_csv {
		let csv = stats_csv(&rows)?;
		return Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], csv).into_response());
	}
	Ok(Json(rows).into_response())
}

/// Subscribe to events. Deliveries are POSTed with `X-Signature: sha256=<hex HMAC-SHA256 of
/// the body under the secret>`, `X-Event` and `X-Delivery-Id`; a non-2xx answer is retried
/// with backoff for about a day. The body is an `EventPayload`.
#[utoipa::path(post, path = "/webhooks", tag = "webhooks", security(("bearer" = [])), request_body = NewWebhook,
	responses((status = 201, body = WebhookDto), (status = 400, body = ErrorBody)))]
async fn add_webhook(State(s): State<AppState>, Json(hook): Json<NewWebhook>) -> ApiResult<(StatusCode, Json<WebhookDto>)> {
	Ok((StatusCode::CREATED, Json(s.archive.add_webhook(&hook).await?)))
}

#[utoipa::path(get, path = "/webhooks", tag = "webhooks", security(("bearer" = [])), responses((status = 200, body = [WebhookDto])))]
async fn webhooks(State(s): State<AppState>) -> ApiResult<Json<Vec<WebhookDto>>> {
	Ok(Json(s.archive.webhooks().await?))
}

/// Removes the hook and whatever it was still owed.
#[utoipa::path(delete, path = "/webhooks/{id}", tag = "webhooks", security(("bearer" = [])), params(("id" = i64, Path)),
	responses((status = 204), (status = 404, body = ErrorBody)))]
async fn delete_webhook(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
	if s.archive.delete_webhook(id).await? {
		Ok(StatusCode::NO_CONTENT)
	} else {
		Err(not_found(format!("no webhook {id}")))
	}
}

#[derive(OpenApi)]
#[openapi(
	info(title = "review_archive", description = "Archive of public place reviews: a PNG of every review as it first appears, plus data for statistics."),
	paths(
		targets, add_target, target, patch_target, delete_target, reviews, runs, scan, capture, job, review, capture_png, export_zip, stats, add_webhook,
		webhooks, delete_webhook
	),
	components(schemas(
		TargetDto, TargetDetail, NewTarget, TargetPatch, RunDto, RunStatus, Counts, JobAccepted, JobDto, JobKind, JobStatus, CaptureRequest, ReviewDto,
		ReviewDetail, VersionDto, CaptureDto, DayStats, NewWebhook, WebhookDto, Event, EventPayload, ErrorBody
	)),
	modifiers(&BearerAuth)
)]
pub struct ApiDoc;

struct BearerAuth;

impl utoipa::Modify for BearerAuth {
	fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
		let components = openapi.components.get_or_insert_with(Default::default);
		components.add_security_scheme("bearer", SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()));
	}
}
