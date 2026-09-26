//! The HTTP API. Everything but `/health` wants the bearer token.

use std::sync::Arc;

use axum::{
	Json, Router,
	extract::{Path, Query, Request, State},
	http::{HeaderMap, StatusCode, header},
	middleware::{self, Next},
	response::{IntoResponse, Response},
	routing::get,
};
use review_archive::Archive;
use review_archive_core::{
	TargetId,
	dto::{TargetDto, stats_csv},
	parse_since,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct AppState {
	pub archive: Archive,
	/// SHA-256 of the token: compared as digests, so the comparison time says nothing about it.
	pub token_digest: Arc<[u8; 32]>,
}

impl AppState {
	pub fn new(archive: Archive, token: &str) -> Self {
		Self {
			archive,
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

pub struct ApiError(StatusCode, String);

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

async fn targets(State(s): State<AppState>) -> Result<Json<Vec<TargetDto>>, ApiError> {
	Ok(Json(s.archive.targets().await?.into_iter().map(TargetDto::from).collect()))
}

#[derive(Deserialize)]
struct ReviewsQuery {
	since: Option<String>,
	gone: Option<bool>,
}

async fn reviews(State(s): State<AppState>, Path(id): Path<i64>, Query(q): Query<ReviewsQuery>) -> Result<Response, ApiError> {
	let since = q.since.as_deref().map(parse_since).transpose().map_err(|e| bad_request(format!("since: {e}")))?;
	let rows = s.archive.reviews(TargetId(id), since, q.gone).await?;
	Ok(Json(rows).into_response())
}

async fn capture(State(s): State<AppState>, Path(file): Path<String>) -> Result<Response, ApiError> {
	let not_found = || ApiError(StatusCode::NOT_FOUND, "no such capture".into());
	let sha = file.strip_suffix(".png").ok_or_else(not_found)?;
	// only what the archive recorded is served, not whatever happens to sit in the blob dir
	let bytes = s.archive.capture_png(sha).await?.ok_or_else(not_found)?;
	Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")], bytes).into_response())
}

#[derive(Deserialize)]
struct StatsQuery {
	target: Option<i64>,
	from: Option<jiff::civil::Date>,
	to: Option<jiff::civil::Date>,
}

async fn stats(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<StatsQuery>) -> Result<Response, ApiError> {
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
