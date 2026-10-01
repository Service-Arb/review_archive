//! Who is calling. The static `REVIEW_ARCHIVE_TOKEN` bearer is the operator (and the services
//! using the client crate). A browser comes with valeratrades.com's `va_access` cookie
//! ([`va_sso`]), verified here with the site's public key: its admins are operators too,
//! and members of `service-arb` get `/me`. On `/me` routes an admin acts as any member, named
//! by `X-Member`.

use axum::{
	Json,
	extract::{FromRequestParts, Request, State},
	http::{HeaderMap, Method, StatusCode, header, request::Parts},
	middleware::Next,
	response::{IntoResponse, Response},
};
use review_archive_core::dto::{ErrorBody, MEMBER_HEADER};
use sha2::{Digest, Sha256};

use crate::http::AppState;

pub(crate) const GROUP: &str = "service-arb";

/// Who a request is from: `email` is a signed-in person, `admin` may use the operator's routes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Caller {
	pub email: Option<String>,
	pub username: Option<String>,
	pub admin: bool,
}

/// valeratrades.com, the sign-in: its public key, and its routes a browser or this server goes to.
pub struct SsoSite {
	verifier: va_sso::Verifier,
	/// `/auth/refresh`: where a browser without a live cookie is sent
	pub refresh: String,
	/// `/auth/members`, beside it
	pub(crate) members: reqwest::Url,
}

impl SsoSite {
	pub fn new(verifier: va_sso::Verifier, refresh: &str) -> eyre::Result<Self> {
		let url: reqwest::Url = refresh.parse().map_err(|e| eyre::eyre!("SSO_REFRESH_URL is not a URL: {refresh}: {e}"))?;
		eyre::ensure!(matches!(url.scheme(), "https" | "http"), "SSO_REFRESH_URL is not an http(s) URL: {refresh}");
		Ok(Self {
			verifier,
			refresh: refresh.to_owned(),
			members: url.join("members").expect("a relative path joins any http(s) URL"),
		})
	}
}

pub struct Auth {
	/// SHA-256 of the admin token: compared as digests, so the comparison time says nothing about it.
	admin_digest: [u8; 32],
	pub(crate) sso: Option<SsoSite>,
}

impl Auth {
	/// `sso`: `None` takes only the operator's token.
	pub fn new(admin_token: &str, sso: Option<SsoSite>) -> Self {
		Self {
			admin_digest: Sha256::digest(admin_token.as_bytes()).into(),
			sso,
		}
	}

	fn caller(&self, headers: &HeaderMap, method: &Method) -> Result<Caller, Response> {
		if let Some(auth) = headers.get(header::AUTHORIZATION) {
			let token = auth.to_str().ok().and_then(|v| v.strip_prefix("Bearer ")).ok_or_else(unauthorized)?;
			let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
			return match digest.iter().zip(self.admin_digest.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0 {
				true => Ok(Caller {
					email: None,
					username: None,
					admin: true,
				}),
				false => Err(unauthorized()),
			};
		}
		let (Some(sso), Some(cookie)) = (&self.sso, cookie(headers, va_sso::COOKIE)) else {
			return Err(unauthorized());
		};
		let claims = sso.verifier.verify(cookie).map_err(|_| unauthorized())?;
		// a cookie rides along on requests other sites start; only this origin's own may write
		if !matches!(*method, Method::GET | Method::HEAD) && headers.get("sec-fetch-site").is_none_or(|v| v != "same-origin") {
			return Err(refuse(StatusCode::FORBIDDEN, "a signed-in write must come from this site's own pages"));
		}
		if !claims.member_of(GROUP) {
			return Err(refuse(StatusCode::FORBIDDEN, &format!("{} is not a {GROUP} member", claims.email)));
		}
		Ok(Caller {
			email: Some(claims.email.to_lowercase()),
			username: Some(claims.username),
			admin: claims.admin,
		})
	}
}

pub(crate) fn cookie<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
	headers
		.get_all(header::COOKIE)
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|v| v.split(';'))
		.find_map(|c| c.trim().strip_prefix(name)?.strip_prefix('='))
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
pub async fn authenticate(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
	match state.auth.caller(req.headers(), req.method()) {
		Ok(caller) => {
			req.extensions_mut().insert(caller);
			next.run(req).await
		}
		Err(r) => r,
	}
}

/// The operator's routes.
pub async fn admin_only(req: Request, next: Next) -> Response {
	match req.extensions().get::<Caller>().expect("layered under authenticate").admin {
		true => next.run(req).await,
		false => refuse(StatusCode::FORBIDDEN, "this route is the operator's"),
	}
}

/// Whose `/me` this is: the signed-in person, or the member an admin names in `X-Member`.
pub struct Member(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Member {
	type Rejection = Response;

	async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Response> {
		let caller = parts.extensions.get::<Caller>().expect("layered under authenticate");
		match (parts.headers.get(MEMBER_HEADER), &caller.email) {
			(Some(m), _) if caller.admin => match m.to_str() {
				Ok(m) if !m.trim().is_empty() => Ok(Self(m.trim().to_lowercase())),
				_ => Err(refuse(StatusCode::BAD_REQUEST, "X-Member is not an email")),
			},
			(Some(_), _) => Err(refuse(StatusCode::FORBIDDEN, "only an admin acts as another member")),
			(None, Some(email)) => Ok(Self(email.clone())),
			(None, None) => Err(refuse(StatusCode::FORBIDDEN, "/me routes are a signed-in member's, not the operator token's")),
		}
	}
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
	type Rejection = std::convert::Infallible;

	async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
		Ok(parts.extensions.get::<Caller>().expect("layered under authenticate").clone())
	}
}
