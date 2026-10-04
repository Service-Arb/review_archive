//! The HTTP API: everything the CLI does, for other services, and `/me` for members; at `/`,
//! the dashboard's page. Everything but `/`, `/health`, `/openapi.json` and the MFE's files
//! wants a caller (see [`crate::auth`]): the operator's routes refuse members, `/me` refuses
//! the operator's token unless it names a member.
//! JSON in and out — errors included, the ones axum's extractors raise too — with the
//! DTOs of `review_archive_core::dto`. Every handler only translates: what to do, and
//! what the caller got wrong, is the facade's.

use std::{io::Seek, sync::Arc, time::Duration};

use axum::{
	Json, Router,
	extract::{
		FromRequest, FromRequestParts, State,
		rejection::{JsonRejection, PathRejection, QueryRejection},
	},
	http::{HeaderMap, StatusCode, header},
	middleware,
	response::{IntoResponse, Response},
	routing::{get, post},
};
use review_archive::{Archive, Rejected, store::export::Destination};
use review_archive_core::{
	ReviewId, TargetId,
	dto::{
		Board, CaptureRequest, DayStats, ErrorBody, EventPayload, ExportQuery, GmailDto, GmailOverview, JobAccepted, JobDto, LedgerEntry, Me, MemberDto, NewGmail, NewTarget, NewTgChannel,
		NewTrack, NewWebhook, ReinstatementDto, ReviewDetail, ReviewDto, ReviewsQuery, RunDto, RunsQuery, StatsQuery, Switch, TargetDetail, TargetDto, TargetPatch, TgChannelDto,
		TokensChange, TokensDto, WaitQuery, WebhookDto, stats_csv,
	},
};
use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::{services::ServeDir, timeout::TimeoutLayer};
use utoipa::{
	OpenApi,
	openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};
use v_utils::{Timeframe, macros::SettingsNested};

use crate::{
	auth::{Auth, Caller, GROUP, Member, admin_only, authenticate, cookie},
	worker::Signals,
};

/// How long requests may take, and how many are served at once.
#[derive(Clone, Debug, Deserialize, Serialize, SettingsNested, SmartDefault, schemars::JsonSchema)]
#[settings(prefix = "http")]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
	/// The longest `POST /captures?wait=` holds a request.
	#[default(Timeframe::from("2m"))]
	pub max_wait: Timeframe,
	/// Past this a request is answered 408: `max_wait` and then some. Exports are exempt.
	#[default(Timeframe::from("3m"))]
	pub request_timeout: Timeframe,
	/// API requests served at once; more wait their turn. `/health` is not counted.
	#[default(64)]
	pub max_concurrent: usize,
}

#[derive(Clone)]
pub struct AppState {
	pub archive: Archive,
	pub auth: Arc<Auth>,
	pub signals: Arc<Signals>,
	pub cfg: Arc<HttpConfig>,
}

impl AppState {
	pub fn new(archive: Archive, auth: Auth, signals: Arc<Signals>, cfg: HttpConfig) -> Self {
		Self {
			archive,
			auth: Arc::new(auth),
			signals,
			cfg: Arc::new(cfg),
		}
	}
}

/// `mfe`: the built dashboard bundle, served under `/mfe/`. `sign_in`: where the page sends a
/// browser without a live `va_access` cookie; with both, `/` is the dashboard's page.
pub fn router(state: AppState, mfe: Option<&std::path::Path>, sign_in: Option<&str>) -> Router {
	let admin = Router::new()
		.route("/targets", get(targets).post(add_target))
		.route("/targets/{id}", get(target).patch(patch_target).delete(delete_target))
		.route("/targets/{id}/reviews", get(reviews))
		.route("/targets/{id}/runs", get(runs))
		.route("/targets/{id}/scan", post(scan))
		.route("/captures", post(capture))
		.route("/jobs/{id}", get(job))
		.route("/reviews/{id}", get(review))
		.route("/stats", get(stats))
		.route("/webhooks", get(webhooks).post(add_webhook))
		.route("/webhooks/{id}", axum::routing::delete(delete_webhook))
		.route("/members", get(members))
		.route("/members/{email}/tokens", post(change_tokens))
		.route_layer(middleware::from_fn(admin_only));
	let me = Router::new()
		.route("/me", get(me))
		.route("/me/tokens", get(ledger))
		.route("/me/overview", get(overview))
		.route("/me/gmails", post(add_gmail))
		.route("/me/gmails/{gmail}", axum::routing::delete(delete_gmail).patch(set_gmail_enabled))
		.route("/me/gmails/{gmail}/tracks", post(track))
		.route("/me/gmails/{gmail}/tracks/{target}", axum::routing::delete(untrack).patch(set_track_enabled))
		.route("/me/gmails/{gmail}/locations/{target}/board", get(board))
		.route("/me/gmails/{gmail}/reinstatements/{review}", axum::routing::put(reinstate).delete(withdraw))
		.route("/me/tg-channels", get(tg_channels).post(add_tg_channel))
		.route("/me/tg-channels/{id}", axum::routing::delete(delete_tg_channel))
		.route("/me/tg-channels/{id}/test", post(test_tg_channel));
	let timed = admin
		.merge(me)
		.route("/captures/{file}", get(capture_avif))
		.layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, state.cfg.request_timeout.duration()));
	// an export of a large archive takes as long as it takes; it streams from a file
	let authed = timed
		.merge(Router::new().route("/targets/{id}/export.zip", get(export_zip)).route_layer(middleware::from_fn(admin_only)))
		.layer(GlobalConcurrencyLimitLayer::new(state.cfg.max_concurrent))
		.route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
	// the probes answer whatever load the API is under
	let mut app = Router::new()
		.route("/health", get(|| async { "ok" }))
		.route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
		.merge(authed);
	if let Some(dir) = mfe {
		let bundle = Router::new()
			.fallback_service(ServeDir::new(dir))
			.layer(middleware::from_fn_with_state(bundle_etag(dir), revalidated));
		app = app.nest_service("/mfe", bundle);
		if let Some(sign_in) = sign_in {
			let page = include_str!("../../review_archive_web/index.html").replace("{sign_in}", sign_in);
			let page = move || {
				let page = page.clone();
				async move { axum::response::Html(page) }
			};
			// the dashboard's own paths: a link to one of its views loads the page, which routes there
			app = app
				.route("/", get(page.clone()))
				.route("/telegram", get(page.clone()))
				.route("/tokens", get(page.clone()))
				.route("/gmails/{*view}", get(page.clone()))
				.route("/members/{member}/{*view}", get(page.clone()))
				.route("/members/{member}/", get(page.clone()))
				.route("/members/{member}", get(page));
		}
	}
	app.with_state(state)
}

/// The whole bundle's digest, for every file in it: the nix store dates each file 1970, so a
/// date tells one build from another for none of them.
fn bundle_etag(dir: &std::path::Path) -> header::HeaderValue {
	use sha2::{Digest, Sha256};
	fn walk(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
		for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading the bundle at {}: {e}", dir.display())) {
			let path = e.expect("listing the bundle").path();
			match path.is_dir() {
				true => walk(&path, files),
				false => files.push(path),
			}
		}
	}
	let mut files = vec![];
	walk(dir, &mut files);
	files.sort();
	let mut h = Sha256::new();
	for f in &files {
		h.update(f.strip_prefix(dir).expect("walked from it").as_os_str().as_encoded_bytes());
		h.update([0]);
		h.update(std::fs::read(f).unwrap_or_else(|e| panic!("reading {}: {e}", f.display())));
	}
	let digest: String = h.finalize()[..16].iter().map(|b| format!("{b:02x}")).collect();
	header::HeaderValue::try_from(format!("\"{digest}\"")).expect("hex is a header value")
}

/// Every bundle file is asked about again on each load, and is the same only while the bundle is.
async fn revalidated(State(etag): State<header::HeaderValue>, mut req: axum::extract::Request, next: middleware::Next) -> Response {
	// an edge that compresses weakens the tag it passes on
	if req
		.headers()
		.get(header::IF_NONE_MATCH)
		.is_some_and(|v| v.as_bytes().strip_prefix(b"W/").unwrap_or(v.as_bytes()) == etag.as_bytes())
	{
		return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
	}
	req.headers_mut().remove(header::IF_MODIFIED_SINCE);
	let mut res = next.run(req).await;
	let h = res.headers_mut();
	h.remove(header::LAST_MODIFIED);
	h.insert(header::ETAG, etag);
	h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-cache"));
	res
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
	fn into_response(self) -> Response {
		(self.0, Json(ErrorBody { error: self.1 })).into_response()
	}
}

impl From<eyre::Report> for ApiError {
	/// The caller's mistakes are 404 / 400 / 429 with the reason. Anything else is a 5xx:
	/// reported, its details kept in the log rather than the response.
	fn from(e: eyre::Report) -> Self {
		match e.downcast_ref::<Rejected>() {
			Some(Rejected::NotFound(m)) => Self(StatusCode::NOT_FOUND, m.clone()),
			Some(Rejected::Invalid(m)) => Self(StatusCode::BAD_REQUEST, m.clone()),
			Some(Rejected::Busy(m)) => Self(StatusCode::TOO_MANY_REQUESTS, m.clone()),
			None => {
				crate::report(&e, "request failed");
				Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
			}
		}
	}
}

/// What axum's extractors refuse is the caller's mistake too, and answered the same way.
macro_rules! rejections {
	($($rejection:ty),*) => {$(
		impl From<$rejection> for ApiError {
			fn from(r: $rejection) -> Self {
				Self(r.status(), r.body_text())
			}
		}
	)*};
}
rejections!(JsonRejection, QueryRejection, PathRejection);

/// A JSON body, refused as an [`ErrorBody`].
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
struct JsonBody<T>(T);

/// A query string, refused as an [`ErrorBody`].
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
struct Query<T>(T);

/// A path parameter, refused as an [`ErrorBody`].
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
struct Path<T>(T);

type ApiResult<T> = Result<T, ApiError>;

#[utoipa::path(get, path = "/targets", tag = "targets", responses((status = 200, body = [TargetDto])))]
async fn targets(State(s): State<AppState>) -> ApiResult<Json<Vec<TargetDto>>> {
	Ok(Json(s.archive.targets().await?.into_iter().map(TargetDto::from).collect()))
}

/// Watch a place. Same resolution as `target add`: a place id, or a Maps URL.
#[utoipa::path(post, path = "/targets", tag = "targets", request_body = NewTarget, responses((status = 201, body = TargetDto), (status = 400, body = ErrorBody)))]
async fn add_target(State(s): State<AppState>, JsonBody(req): JsonBody<NewTarget>) -> ApiResult<(StatusCode, Json<TargetDto>)> {
	Ok((StatusCode::CREATED, Json(s.archive.add_target(&req).await?.target.into())))
}

/// A target with its last run, what is archived, and the next scheduled scan.
#[utoipa::path(get, path = "/targets/{id}", tag = "targets", params(("id" = i64, Path)), responses((status = 200, body = TargetDetail), (status = 404, body = ErrorBody)))]
async fn target(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<TargetDetail>> {
	Ok(Json(s.archive.target_detail(TargetId(id)).await?))
}

#[utoipa::path(patch, path = "/targets/{id}", tag = "targets", params(("id" = i64, Path)), request_body = TargetPatch,
	responses((status = 200, body = TargetDto), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn patch_target(State(s): State<AppState>, Path(id): Path<i64>, JsonBody(patch): JsonBody<TargetPatch>) -> ApiResult<Json<TargetDto>> {
	Ok(Json(s.archive.update_target(TargetId(id), &patch).await?.into()))
}

/// Disables the target. Nothing archived is deleted.
#[utoipa::path(delete, path = "/targets/{id}", tag = "targets", params(("id" = i64, Path)), responses((status = 200, body = TargetDto), (status = 404, body = ErrorBody)))]
async fn delete_target(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<TargetDto>> {
	let disable = TargetPatch {
		enabled: Some(false),
		..Default::default()
	};
	Ok(Json(s.archive.update_target(TargetId(id), &disable).await?.into()))
}

#[utoipa::path(get, path = "/targets/{id}/reviews", tag = "reviews", params(("id" = i64, Path), ReviewsQuery),
	responses((status = 200, body = [ReviewDto]), (status = 404, body = ErrorBody)))]
async fn reviews(State(s): State<AppState>, Path(id): Path<i64>, Query(q): Query<ReviewsQuery>) -> ApiResult<Json<Vec<ReviewDto>>> {
	Ok(Json(s.archive.reviews(TargetId(id), &q).await?))
}

#[utoipa::path(get, path = "/targets/{id}/runs", tag = "targets", params(("id" = i64, Path), RunsQuery), responses((status = 200, body = [RunDto]), (status = 404, body = ErrorBody)))]
async fn runs(State(s): State<AppState>, Path(id): Path<i64>, Query(q): Query<RunsQuery>) -> ApiResult<Json<Vec<RunDto>>> {
	Ok(Json(s.archive.runs(TargetId(id), &q).await?))
}

/// Scan the target now, ahead of the scheduled scans. The same scan already queued is not
/// queued twice.
#[utoipa::path(post, path = "/targets/{id}/scan", tag = "jobs", params(("id" = i64, Path)),
	responses((status = 202, body = JobAccepted), (status = 404, body = ErrorBody), (status = 429, body = ErrorBody)))]
async fn scan(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<(StatusCode, Json<JobAccepted>)> {
	let job_id = s.archive.enqueue_scan(TargetId(id)).await?;
	s.signals.job_queued.notify_one();
	Ok((StatusCode::ACCEPTED, Json(JobAccepted { job_id })))
}

/// Capture a place without registering it. Its results are kept under an implicit,
/// disabled target (the place's existing one, if any), so nothing is lost.
#[utoipa::path(post, path = "/captures", tag = "jobs", params(WaitQuery), request_body = CaptureRequest,
	responses((status = 202, body = JobAccepted, description = "queued, or still running when `wait` ran out"),
	          (status = 200, body = JobDto, description = "finished within `wait`"), (status = 400, body = ErrorBody), (status = 429, body = ErrorBody)))]
async fn capture(State(s): State<AppState>, Query(q): Query<WaitQuery>, JsonBody(req): JsonBody<CaptureRequest>) -> ApiResult<Response> {
	// subscribed before queueing, so the job cannot end unseen in between
	let mut finished = s.signals.job_finished.subscribe();
	let job_id = s.archive.enqueue_capture(&req).await?;
	s.signals.job_queued.notify_one();
	let deadline = tokio::time::Instant::now() + Duration::from_secs(q.wait.unwrap_or(0)).min(s.cfg.max_wait.duration());
	loop {
		let job = s.archive.job(job_id).await?;
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
#[utoipa::path(get, path = "/jobs/{id}", tag = "jobs", params(("id" = i64, Path)), responses((status = 200, body = JobDto), (status = 404, body = ErrorBody)))]
async fn job(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<JobDto>> {
	Ok(Json(s.archive.job(id).await?))
}

/// A review with every version and capture.
#[utoipa::path(get, path = "/reviews/{id}", tag = "reviews", params(("id" = i64, Path)), responses((status = 200, body = ReviewDetail), (status = 404, body = ErrorBody)))]
async fn review(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<ReviewDetail>> {
	Ok(Json(s.archive.review(ReviewId(id)).await?))
}

/// A capture's AVIF, provenance in its Exif. Only what the archive recorded; for a
/// member, only what shows a review of a place they track.
#[utoipa::path(get, path = "/captures/{sha256}.avif", tag = "reviews", params(("sha256" = String, Path)),
	responses((status = 200, content_type = "image/avif"), (status = 404, body = ErrorBody)))]
async fn capture_avif(State(s): State<AppState>, caller: Caller, Path(file): Path<String>) -> ApiResult<Response> {
	let sha = file.strip_suffix(".avif").ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such capture".into()))?;
	let avif = match (caller.admin, caller.email) {
		(true, _) => s.archive.capture_avif(sha).await?,
		(false, Some(m)) => s.archive.member_capture_avif(&m, sha).await?,
		(false, None) => unreachable!("a caller is an admin or signed in"),
	};
	// behind auth, so no shared cache may keep it; the name is its hash, so it never changes
	Ok(([(header::CONTENT_TYPE, "image/avif"), (header::CACHE_CONTROL, "private, max-age=31536000, immutable")], avif).into_response())
}

/// The same archive as `export`: `manifest.json` and the first capture of each review.
#[utoipa::path(get, path = "/targets/{id}/export.zip", tag = "reviews", params(("id" = i64, Path), ExportQuery),
	responses((status = 200, content_type = "application/zip"), (status = 404, body = ErrorBody)))]
async fn export_zip(State(s): State<AppState>, Path(id): Path<i64>, Query(q): Query<ExportQuery>) -> ApiResult<Response> {
	// An archive of thousands of captures does not belong in memory: it is written to a temp file
	// with no name, which goes away with its last handle whatever happens to the request.
	let mut file = tempfile::tempfile().map_err(eyre::Report::new)?;
	s.archive.export(TargetId(id), &q, Destination::Zip(file.try_clone().map_err(eyre::Report::new)?)).await?;
	file.rewind().map_err(eyre::Report::new)?;
	let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file)));
	Ok((
		[
			(header::CONTENT_TYPE, "application/zip".to_owned()),
			(header::CONTENT_DISPOSITION, format!("attachment; filename=\"target-{id}.zip\"")),
		],
		body,
	)
		.into_response())
}

/// Per target and UTC day: new, changed, gone, mean rating, histogram. `Accept: text/csv`
/// for CSV.
#[utoipa::path(get, path = "/stats", tag = "reviews", params(StatsQuery), responses((status = 200, body = [DayStats]), (status = 400, body = ErrorBody)))]
async fn stats(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<StatsQuery>) -> ApiResult<Response> {
	let rows = s.archive.stats(&q).await?;
	let wants_csv = headers
		.get(header::ACCEPT)
		.and_then(|v| v.to_str().ok())
		.is_some_and(|a| a.split(',').any(|m| m.trim().starts_with("text/csv")));
	if wants_csv {
		return Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], stats_csv(&rows)?).into_response());
	}
	Ok(Json(rows).into_response())
}

/// Subscribe to events. Deliveries are POSTed with `X-Signature: sha256=<hex HMAC-SHA256 of
/// the body under the secret>`, `X-Event` and `X-Delivery-Id`; a non-2xx answer is retried
/// with backoff for about a day. The body is an `EventPayload`. The URL may not point at a
/// local or private address unless the archive's `webhooks.allowed_hosts` names its host.
#[utoipa::path(post, path = "/webhooks", tag = "webhooks", request_body = NewWebhook, responses((status = 201, body = WebhookDto), (status = 400, body = ErrorBody)))]
async fn add_webhook(State(s): State<AppState>, JsonBody(hook): JsonBody<NewWebhook>) -> ApiResult<(StatusCode, Json<WebhookDto>)> {
	Ok((StatusCode::CREATED, Json(s.archive.add_webhook(&hook).await?)))
}

#[utoipa::path(get, path = "/webhooks", tag = "webhooks", responses((status = 200, body = [WebhookDto])))]
async fn webhooks(State(s): State<AppState>) -> ApiResult<Json<Vec<WebhookDto>>> {
	Ok(Json(s.archive.webhooks().await?))
}

/// Removes the hook and whatever it was still owed.
#[utoipa::path(delete, path = "/webhooks/{id}", tag = "webhooks", params(("id" = i64, Path)), responses((status = 204), (status = 404, body = ErrorBody)))]
async fn delete_webhook(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
	s.archive.delete_webhook(id).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// Who is signed in, whoever `X-Member` names.
#[utoipa::path(get, path = "/me", tag = "me", responses((status = 200, body = Me), (status = 403, body = ErrorBody)))]
async fn me(State(s): State<AppState>, caller: Caller) -> ApiResult<Json<Me>> {
	match (caller.email, caller.username) {
		(Some(email), Some(username)) => Ok(Json(Me {
			tokens: s.archive.tokens(&email).await?,
			email,
			username,
			admin: caller.admin,
		})),
		_ => Err(ApiError(StatusCode::FORBIDDEN, "the operator's token is no one".into())),
	}
}

/// Everyone in `service-arb`, as valeratrades.com lists them, asked with the admin's own sign-in.
#[utoipa::path(get, path = "/members", tag = "me", responses((status = 200, body = [MemberDto]), (status = 403, body = ErrorBody), (status = 502, body = ErrorBody)))]
async fn members(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Vec<MemberDto>>> {
	let (Some(sso), Some(access)) = (&s.auth.sso, cookie(&headers, va_sso::COOKIE)) else {
		return Err(ApiError(StatusCode::FORBIDDEN, "the site lists members to a signed-in admin, not to the operator's token".into()));
	};
	let bad_gateway = |e: eyre::Report| {
		crate::report(&e, "listing members");
		ApiError(StatusCode::BAD_GATEWAY, "valeratrades.com did not list the members".into())
	};
	let resp = reqwest::Client::new()
		.get(sso.members.clone())
		.query(&[("group", GROUP)])
		.header(header::COOKIE, format!("{}={access}", va_sso::COOKIE))
		.timeout(Duration::from_secs(10))
		.send()
		.await
		.map_err(|e| bad_gateway(e.into()))?;
	let status = resp.status();
	if status.is_success() {
		#[derive(Deserialize)]
		struct Listed {
			email: String,
			username: Option<String>,
			display_name: Option<String>,
		}
		let listed: Vec<Listed> = resp.json().await.map_err(|e| bad_gateway(e.into()))?;
		let mut out = Vec::with_capacity(listed.len());
		for m in listed {
			out.push(MemberDto {
				balance: s.archive.tokens(&m.email.to_lowercase()).await?.balance,
				email: m.email,
				username: m.username,
				display_name: m.display_name,
			});
		}
		return Ok(Json(out));
	}
	let why = resp.text().await.map_err(|e| bad_gateway(e.into()))?;
	match status {
		reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => Err(ApiError(StatusCode::from_u16(status.as_u16()).expect("401 and 403 are statuses"), why)),
		_ => Err(bad_gateway(eyre::eyre!("{} answered {status}: {why}", sso.members))),
	}
}

/// Sets a member's balance, or adds to it (a grant, a purchase); an admin's.
#[utoipa::path(post, path = "/members/{email}/tokens", tag = "me", params(("email" = String, Path)), request_body = TokensChange,
	responses((status = 200, body = TokensDto), (status = 400, body = ErrorBody), (status = 403, body = ErrorBody)))]
async fn change_tokens(State(s): State<AppState>, caller: Caller, Path(email): Path<String>, JsonBody(req): JsonBody<TokensChange>) -> ApiResult<Json<TokensDto>> {
	let by = caller.email.as_deref().unwrap_or("operator");
	Ok(Json(s.archive.change_tokens(&email.trim().to_lowercase(), &req, by).await?))
}

/// The member's token ledger, newest first; each charge with its run and place.
#[utoipa::path(get, path = "/me/tokens", tag = "me", responses((status = 200, body = [LedgerEntry])))]
async fn ledger(State(s): State<AppState>, Member(m): Member) -> ApiResult<Json<Vec<LedgerEntry>>> {
	Ok(Json(s.archive.ledger(&m).await?))
}

/// The member's gmails, each with its places, by screenshots over the last 7 days.
#[utoipa::path(get, path = "/me/overview", tag = "me", responses((status = 200, body = [GmailOverview])))]
async fn overview(State(s): State<AppState>, Member(m): Member) -> ApiResult<Json<Vec<GmailOverview>>> {
	Ok(Json(s.archive.overview(&m).await?))
}

/// Adds a managing gmail: the Google account a group of the member's places is managed from.
#[utoipa::path(post, path = "/me/gmails", tag = "me", request_body = NewGmail, responses((status = 201, body = GmailDto), (status = 400, body = ErrorBody)))]
async fn add_gmail(State(s): State<AppState>, Member(m): Member, JsonBody(req): JsonBody<NewGmail>) -> ApiResult<(StatusCode, Json<GmailDto>)> {
	Ok((StatusCode::CREATED, Json(s.archive.add_gmail(&m, &req).await?)))
}

/// Removes a gmail with its tracks and the Telegram channels scoped to it. A gmail with
/// reinstatement requests on record is kept (400).
#[utoipa::path(delete, path = "/me/gmails/{gmail}", tag = "me", params(("gmail" = i64, Path)), responses((status = 204), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn delete_gmail(State(s): State<AppState>, Member(m): Member, Path(gmail): Path<i64>) -> ApiResult<StatusCode> {
	s.archive.delete_gmail(&m, gmail).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// Switches a gmail on or off. Its places are scanned while some member has them on under a gmail that is on.
#[utoipa::path(patch, path = "/me/gmails/{gmail}", tag = "me", params(("gmail" = i64, Path)), request_body = Switch, responses((status = 204), (status = 404, body = ErrorBody)))]
async fn set_gmail_enabled(State(s): State<AppState>, Member(m): Member, Path(gmail): Path<i64>, JsonBody(req): JsonBody<Switch>) -> ApiResult<StatusCode> {
	s.archive.set_gmail_enabled(&m, gmail, req.enabled).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// Tracks a place under the gmail. A place already watched (same place, language and
/// source) is shared, not scanned twice.
#[utoipa::path(post, path = "/me/gmails/{gmail}/tracks", tag = "me", params(("gmail" = i64, Path)), request_body = NewTrack,
	responses((status = 201, body = TargetDto), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn track(State(s): State<AppState>, Member(m): Member, Path(gmail): Path<i64>, JsonBody(req): JsonBody<NewTrack>) -> ApiResult<(StatusCode, Json<TargetDto>)> {
	Ok((StatusCode::CREATED, Json(s.archive.track(&m, gmail, &req).await?)))
}

/// Stops tracking; the place and its archive stay.
#[utoipa::path(delete, path = "/me/gmails/{gmail}/tracks/{target}", tag = "me", params(("gmail" = i64, Path), ("target" = i64, Path)),
	responses((status = 204), (status = 404, body = ErrorBody)))]
async fn untrack(State(s): State<AppState>, Member(m): Member, Path((gmail, target)): Path<(i64, i64)>) -> ApiResult<StatusCode> {
	s.archive.untrack(&m, gmail, TargetId(target)).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// Switches the gmail's track of a place on or off; the place's target stays as it is.
#[utoipa::path(patch, path = "/me/gmails/{gmail}/tracks/{target}", tag = "me", params(("gmail" = i64, Path), ("target" = i64, Path)), request_body = Switch,
	responses((status = 204), (status = 404, body = ErrorBody)))]
async fn set_track_enabled(State(s): State<AppState>, Member(m): Member, Path((gmail, target)): Path<(i64, i64)>, JsonBody(req): JsonBody<Switch>) -> ApiResult<StatusCode> {
	s.archive.set_track_enabled(&m, gmail, TargetId(target), req.enabled).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// A tracked place's reviews in three columns: snapshotted, removed, reinstating.
#[utoipa::path(get, path = "/me/gmails/{gmail}/locations/{target}/board", tag = "me", params(("gmail" = i64, Path), ("target" = i64, Path)),
	responses((status = 200, body = Board), (status = 404, body = ErrorBody)))]
async fn board(State(s): State<AppState>, Member(m): Member, Path((gmail, target)): Path<(i64, i64)>) -> ApiResult<Json<Board>> {
	Ok(Json(s.archive.board(&m, gmail, TargetId(target)).await?))
}

/// Records that the removed review's reinstatement was asked of Google, now. Asking again
/// while one is open answers the open one.
#[utoipa::path(put, path = "/me/gmails/{gmail}/reinstatements/{review}", tag = "me", params(("gmail" = i64, Path), ("review" = i64, Path)),
	responses((status = 200, body = ReinstatementDto), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn reinstate(State(s): State<AppState>, Member(m): Member, Path((gmail, review)): Path<(i64, i64)>) -> ApiResult<Json<ReinstatementDto>> {
	Ok(Json(s.archive.reinstate(&m, gmail, ReviewId(review)).await?))
}

/// Withdraws the open request; it stays on record as withdrawn.
#[utoipa::path(delete, path = "/me/gmails/{gmail}/reinstatements/{review}", tag = "me", params(("gmail" = i64, Path), ("review" = i64, Path)),
	responses((status = 204), (status = 404, body = ErrorBody)))]
async fn withdraw(State(s): State<AppState>, Member(m): Member, Path((gmail, review)): Path<(i64, i64)>) -> ApiResult<StatusCode> {
	s.archive.withdraw_reinstatement(&m, gmail, ReviewId(review)).await?;
	Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/me/tg-channels", tag = "me", responses((status = 200, body = [TgChannelDto])))]
async fn tg_channels(State(s): State<AppState>, Member(m): Member) -> ApiResult<Json<Vec<TgChannelDto>>> {
	Ok(Json(s.archive.tg_channels(&m).await?))
}

/// Sends the events of the member's places to a Telegram chat, through the archive's bot —
/// which has to be in the chat, allowed to post. A removed review comes with its screenshot.
#[utoipa::path(post, path = "/me/tg-channels", tag = "me", request_body = NewTgChannel, responses((status = 201, body = TgChannelDto), (status = 400, body = ErrorBody)))]
async fn add_tg_channel(State(s): State<AppState>, Member(m): Member, JsonBody(req): JsonBody<NewTgChannel>) -> ApiResult<(StatusCode, Json<TgChannelDto>)> {
	Ok((StatusCode::CREATED, Json(s.archive.add_tg_channel(&m, &req).await?)))
}

/// Removes a channel and what it was still owed.
#[utoipa::path(delete, path = "/me/tg-channels/{id}", tag = "me", params(("id" = i64, Path)), responses((status = 204), (status = 404, body = ErrorBody)))]
async fn delete_tg_channel(State(s): State<AppState>, Member(m): Member, Path(id): Path<i64>) -> ApiResult<StatusCode> {
	s.archive.delete_tg_channel(&m, id).await?;
	Ok(StatusCode::NO_CONTENT)
}

/// Posts a line to the channel now. 400 with Telegram's reason when it refuses.
#[utoipa::path(post, path = "/me/tg-channels/{id}/test", tag = "me", params(("id" = i64, Path)),
	responses((status = 204), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn test_tg_channel(State(s): State<AppState>, Member(m): Member, Path(id): Path<i64>) -> ApiResult<StatusCode> {
	s.archive.test_tg_channel(&m, id).await?;
	Ok(StatusCode::NO_CONTENT)
}

#[derive(OpenApi)]
#[openapi(
	info(title = "review_archive", description = "Archive of public place reviews: an AVIF screenshot of every review as it first appears, plus data for statistics."),
	paths(
		targets, add_target, target, patch_target, delete_target, reviews, runs, scan, capture, job, review, capture_avif, export_zip, stats, add_webhook,
		webhooks, delete_webhook
	),
	// what no path returns: the body of a webhook delivery
	components(schemas(EventPayload)),
	security(("bearer" = [])),
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
