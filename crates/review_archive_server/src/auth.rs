//! Who is calling. The static `REVIEW_ARCHIVE_TOKEN` is the operator (and the services
//! using the client crate): everything. Any other bearer is a playbook access token, asked
//! of playbook's introspection endpoint and answered with a member's email — kept for a
//! minute at most, so a revoked membership stops working within one.

use std::{
	collections::HashMap,
	sync::Mutex,
	time::{Duration, Instant},
};

use axum::{
	Json,
	extract::{FromRequestParts, Request, State},
	http::{HeaderMap, StatusCode, header, request::Parts},
	middleware::Next,
	response::{IntoResponse, Response},
};
use review_archive_core::dto::ErrorBody;
use sha2::{Digest, Sha256};

use crate::http::AppState;

/// How long an introspection answer is trusted, at most.
const CACHE_FOR: Duration = Duration::from_secs(60);
/// Past this many cached tokens, the expired ones are dropped on the next insert.
const CACHE_PRUNE_AT: usize = 1024;

/// Who a request is from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Caller {
	/// Holds `REVIEW_ARCHIVE_TOKEN`.
	Admin,
	/// A playbook member, by email.
	Member(String),
}

/// Checks bearers.
pub struct Auth {
	/// SHA-256 of the admin token: compared as digests, so the comparison time says nothing about it.
	admin_digest: [u8; 32],
	introspect: Option<Introspect>,
}

/// Playbook's introspection endpoint and this service's secret for it.
pub struct Introspect {
	url: reqwest::Url,
	secret: String,
	http: reqwest::Client,
	/// By token digest: the member, and until when that answer holds.
	cache: Mutex<HashMap<[u8; 32], (String, Instant)>>,
}

impl Introspect {
	pub fn new(url: reqwest::Url, secret: String) -> eyre::Result<Self> {
		Ok(Self {
			url,
			secret,
			http: reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?,
			cache: Mutex::default(),
		})
	}

	/// The member the token belongs to; `None` for a token playbook does not vouch for.
	async fn member(&self, token: &str, digest: [u8; 32]) -> eyre::Result<Option<String>> {
		let now = Instant::now();
		if let Some((email, until)) = self.cache.lock().expect("nothing under this lock panics").get(&digest)
			&& *until > now
		{
			return Ok(Some(email.clone()));
		}
		#[derive(serde::Deserialize)]
		struct Answer {
			active: bool,
			email: Option<String>,
			exp: Option<i64>,
		}
		let resp = self.http.post(self.url.clone()).bearer_auth(&self.secret).form(&[("token", token)]).send().await?;
		eyre::ensure!(resp.status().is_success(), "introspection at {} answered {}", self.url, resp.status());
		let a: Answer = resp.json().await?;
		if !a.active {
			return Ok(None);
		}
		let email = a.email.ok_or_else(|| eyre::eyre!("introspection vouched for a token without saying whose"))?.to_lowercase();
		let left = a
			.exp
			.map(|exp| Duration::from_secs(u64::try_from(exp - jiff::Timestamp::now().as_second()).unwrap_or(0)))
			.map_or(CACHE_FOR, |d| d.min(CACHE_FOR));
		let mut cache = self.cache.lock().expect("nothing under this lock panics");
		if cache.len() >= CACHE_PRUNE_AT {
			cache.retain(|_, (_, until)| *until > now);
		}
		cache.insert(digest, (email.clone(), now + left));
		Ok(Some(email))
	}
}

impl Auth {
	pub fn new(admin_token: &str, introspect: Option<Introspect>) -> Self {
		Self {
			admin_digest: Sha256::digest(admin_token.as_bytes()).into(),
			introspect,
		}
	}

	async fn caller(&self, token: &str) -> Result<Caller, Response> {
		let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
		if digest.iter().zip(self.admin_digest.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0 {
			return Ok(Caller::Admin);
		}
		let Some(introspect) = &self.introspect else { return Err(unauthorized()) };
		match introspect.member(token, digest).await {
			Ok(Some(email)) => Ok(Caller::Member(email)),
			Ok(None) => Err(unauthorized()),
			Err(e) => {
				crate::report(&e, "introspection failed");
				Err(refuse(StatusCode::SERVICE_UNAVAILABLE, "the authorization server could not be asked; try again"))
			}
		}
	}
}

fn refuse(status: StatusCode, why: &str) -> Response {
	(status, Json(ErrorBody { error: why.into() })).into_response()
}

fn unauthorized() -> Response {
	let mut r = refuse(StatusCode::UNAUTHORIZED, "unauthorized");
	r.headers_mut().insert(header::WWW_AUTHENTICATE, header::HeaderValue::from_static("Bearer"));
	r
}

/// Every authenticated route: puts the [`Caller`] in the request.
pub async fn authenticate(State(state): State<AppState>, headers: HeaderMap, mut req: Request, next: Next) -> Response {
	let Some(token) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) else {
		return unauthorized();
	};
	match state.auth.caller(token).await {
		Ok(caller) => {
			req.extensions_mut().insert(caller);
			next.run(req).await
		}
		Err(r) => r,
	}
}

/// The operator's routes.
pub async fn admin_only(req: Request, next: Next) -> Response {
	match req.extensions().get::<Caller>() {
		Some(Caller::Admin) => next.run(req).await,
		Some(Caller::Member(_)) => refuse(StatusCode::FORBIDDEN, "this route is the operator's"),
		None => unreachable!("layered under authenticate"),
	}
}

/// The calling member, for `/me` routes.
pub struct Member(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Member {
	type Rejection = Response;

	async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Response> {
		match parts.extensions.get::<Caller>() {
			Some(Caller::Member(email)) => Ok(Self(email.clone())),
			Some(Caller::Admin) => Err(refuse(StatusCode::FORBIDDEN, "/me routes take a member's token, not the operator's")),
			None => unreachable!("layered under authenticate"),
		}
	}
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
	type Rejection = std::convert::Infallible;

	async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
		Ok(parts.extensions.get::<Caller>().expect("layered under authenticate").clone())
	}
}
