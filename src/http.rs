//! The read-only HTTP API. Everything but `/health` wants the bearer token.

use std::sync::Arc;

use axum::{
	Json, Router,
	extract::{Path, Query, Request, State},
	http::{HeaderMap, StatusCode, header},
	middleware::{self, Next},
	response::{IntoResponse, Response},
	routing::get,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
	domain::{Target, TargetId},
	store::{DayStats, Store, blobs::BlobStore},
};

#[derive(Clone)]
pub struct AppState {
	pub store: Store,
	pub blobs: BlobStore,
	/// SHA-256 of the token: compared as digests, so the comparison time says nothing about it.
	pub token_digest: Arc<[u8; 32]>,
}

impl AppState {
	pub fn new(store: Store, blobs: BlobStore, token: &str) -> Self {
		Self {
			store,
			blobs,
			token_digest: Arc::new(Sha256::digest(token.as_bytes()).into()),
		}
	}
}

pub fn router(state: AppState) -> Router {
	let authed = Router::new()
		.route("/targets", get(targets))
		.route("/targets/{id}/reviews", get(reviews))
		.route("/captures/{file}", get(capture))
		.route("/stats", get(stats))
		.route_layer(middleware::from_fn_with_state(state.clone(), auth));
	Router::new().route("/health", get(|| async { "ok" })).merge(authed).with_state(state)
}

async fn auth(State(state): State<AppState>, headers: HeaderMap, req: Request, next: Next) -> Response {
	let given = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
	let ok = given.is_some_and(|t| {
		let d: [u8; 32] = Sha256::digest(t.as_bytes()).into();
		d.iter().zip(state.token_digest.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
	});
	if !ok {
		return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "unauthorized").into_response();
	}
	next.run(req).await
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
	fn into_response(self) -> Response {
		(self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
	}
}

impl From<eyre::Report> for ApiError {
	fn from(e: eyre::Report) -> Self {
		tracing::error!(error = %format!("{e:#}"), "request failed");
		Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
	}
}

fn bad_request(msg: impl Into<String>) -> ApiError {
	ApiError(StatusCode::BAD_REQUEST, msg.into())
}

#[derive(Serialize)]
struct TargetDto {
	id: i64,
	label: String,
	kind: &'static str,
	place_id: String,
	gbp_account: Option<String>,
	gbp_location: Option<String>,
	lang: String,
	interval_secs: u64,
	enabled: bool,
	created_at: String,
}

impl From<Target> for TargetDto {
	fn from(t: Target) -> Self {
		Self {
			id: t.id.0,
			label: t.label,
			kind: t.kind.as_str(),
			place_id: t.place_id,
			gbp_account: t.gbp.as_ref().map(|g| g.account.clone()),
			gbp_location: t.gbp.map(|g| g.location),
			lang: t.lang,
			interval_secs: t.interval.as_secs(),
			enabled: t.enabled,
			created_at: crate::store::fmt_ts(t.created_at),
		}
	}
}

async fn targets(State(s): State<AppState>) -> Result<Json<Vec<TargetDto>>, ApiError> {
	Ok(Json(s.store.targets().await?.into_iter().map(TargetDto::from).collect()))
}

#[derive(Deserialize)]
struct ReviewsQuery {
	since: Option<String>,
	gone: Option<bool>,
}

async fn reviews(State(s): State<AppState>, Path(id): Path<i64>, Query(q): Query<ReviewsQuery>) -> Result<Response, ApiError> {
	let since = q.since.as_deref().map(parse_since).transpose().map_err(|e| bad_request(format!("since: {e}")))?;
	let rows = s.store.reviews(TargetId(id), since, q.gone).await?;
	Ok(Json(rows).into_response())
}

async fn capture(State(s): State<AppState>, Path(file): Path<String>) -> Result<Response, ApiError> {
	let not_found = || ApiError(StatusCode::NOT_FOUND, "no such capture".into());
	let sha = file.strip_suffix(".png").ok_or_else(not_found)?;
	let path = s.blobs.path_of(sha).ok_or_else(not_found)?;
	// only what the archive recorded is served, not whatever happens to sit in the blob dir
	if !s.store.capture_exists(sha).await? {
		return Err(not_found());
	}
	let bytes = match tokio::fs::read(&path).await {
		Ok(b) => b,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_found()),
		Err(e) => return Err(eyre::Report::new(e).wrap_err(format!("reading {}", path.display())).into()),
	};
	Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")], bytes).into_response())
}

#[derive(Deserialize)]
struct StatsQuery {
	target: Option<i64>,
	from: Option<jiff::civil::Date>,
	to: Option<jiff::civil::Date>,
}

async fn stats(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<StatsQuery>) -> Result<Response, ApiError> {
	let rows = s.store.stats(q.target.map(TargetId), q.from, q.to).await?;
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

/// A date (`2026-09-01`, from its start in UTC) or a full timestamp.
pub fn parse_since(s: &str) -> eyre::Result<jiff::Timestamp> {
	if let Ok(t) = s.parse::<jiff::Timestamp>() {
		return Ok(t);
	}
	let d: jiff::civil::Date = s.parse().map_err(|_| eyre::eyre!("expected YYYY-MM-DD or an RFC 3339 timestamp, got {s:?}"))?;
	Ok(d.to_zoned(jiff::tz::TimeZone::UTC)?.timestamp())
}
