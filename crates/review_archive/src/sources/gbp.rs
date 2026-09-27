//! `gbp`: profiles we manage, from the official Business Profile API. The list is
//! authoritative; the screenshots still come from the public Maps page.
//!
//! Read-only: the only calls made are the OAuth token refresh and `reviews.list`.

use std::collections::HashSet;

use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{
	GbpLocation, Known, Observed, Scan, Target,
	gbp::{FindCards, ReviewList, ReviewsPage, list_coverage, match_captures, observed},
	maps::WalkPolicy,
};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::ReviewSource;
use crate::{browser::Browser, config::Defaults};

/// Google's OAuth token endpoint.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// The Business Profile API.
pub const API_BASE: &str = "https://mybusiness.googleapis.com";
const PAGE_SIZE: u32 = 50;
/// Pages read at most: 50 000 reviews, far past any real location. An API that keeps
/// answering with a next page is not followed forever.
const MAX_PAGES: usize = 1000;

/// An OAuth client and a refresh token with the `business.manage` scope.
#[derive(Clone)]
pub struct Credentials {
	/// `GBP_CLIENT_ID`.
	pub client_id: String,
	/// `GBP_CLIENT_SECRET`.
	pub client_secret: String,
	/// `GBP_REFRESH_TOKEN`.
	pub refresh_token: String,
}

impl std::fmt::Debug for Credentials {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Credentials").field("client_id", &self.client_id).finish_non_exhaustive()
	}
}

/// The Business Profile API, read-only.
#[derive(Debug)]
pub struct Client {
	http: reqwest::Client,
	creds: Credentials,
	token_url: String,
	api_base: String,
	token: Mutex<Option<String>>,
}

impl Client {
	/// A client against Google.
	pub fn new(http: reqwest::Client, creds: Credentials) -> Self {
		Self::with_endpoints(http, creds, TOKEN_URL, API_BASE)
	}

	/// A client against other endpoints (a stub, in tests).
	pub fn with_endpoints(http: reqwest::Client, creds: Credentials, token_url: &str, api_base: &str) -> Self {
		Self {
			http,
			creds,
			token_url: token_url.to_owned(),
			api_base: api_base.trim_end_matches('/').to_owned(),
			token: Mutex::new(None),
		}
	}

	/// Every review of the location, all pages.
	pub async fn reviews(&self, loc: &GbpLocation) -> eyre::Result<ReviewList> {
		let url = format!("{}/v4/accounts/{}/locations/{}/reviews", self.api_base, loc.account, loc.location);
		let mut out = ReviewList { reviews: Vec::new(), total: None };
		let mut page_token: Option<String> = None;
		let mut tokens = HashSet::new();
		for _ in 0..MAX_PAGES {
			let page: ReviewsPage = self.get(&url, page_token.as_deref()).await?;
			out.reviews.extend(page.reviews);
			out.total = out.total.or(page.total_review_count);
			match page.next_page_token.filter(|t| !t.is_empty()) {
				Some(t) if !tokens.insert(t.clone()) => eyre::bail!("GET {url}: the API returned page token {t:?} twice"),
				Some(t) => page_token = Some(t),
				None => return Ok(out),
			}
		}
		eyre::bail!("GET {url}: still more pages after {MAX_PAGES}")
	}

	async fn get<T: serde::de::DeserializeOwned>(&self, url: &str, page_token: Option<&str>) -> eyre::Result<T> {
		// One retry with a fresh token: an access token can expire between two pages.
		for attempt in 0..2 {
			let token = self.access_token(attempt > 0).await?;
			let mut req = self.http.get(url).bearer_auth(&token).query(&[("pageSize", PAGE_SIZE.to_string())]);
			if let Some(t) = page_token {
				req = req.query(&[("pageToken", t)]);
			}
			let resp = req.send().await.wrap_err_with(|| format!("GET {url}"))?;
			let status = resp.status();
			if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
				continue;
			}
			if !status.is_success() {
				let body = resp.text().await.unwrap_or_default();
				eyre::bail!("GET {url}: {status}: {body}");
			}
			return resp.json().await.wrap_err_with(|| format!("decoding {url}"));
		}
		eyre::bail!("GET {url}: still unauthorized after refreshing the access token")
	}

	async fn access_token(&self, force_refresh: bool) -> eyre::Result<String> {
		#[derive(Deserialize)]
		struct TokenResp {
			access_token: String,
		}
		let mut cached = self.token.lock().await;
		if let Some(t) = cached.as_ref().filter(|_| !force_refresh) {
			return Ok(t.clone());
		}
		let resp = self
			.http
			.post(&self.token_url)
			.form(&[
				("grant_type", "refresh_token"),
				("client_id", self.creds.client_id.as_str()),
				("client_secret", self.creds.client_secret.as_str()),
				("refresh_token", self.creds.refresh_token.as_str()),
			])
			.send()
			.await
			.wrap_err("refreshing the GBP access token")?;
		let status = resp.status();
		if !status.is_success() {
			let body = resp.text().await.unwrap_or_default();
			eyre::bail!("refreshing the GBP access token: {status}: {body}");
		}
		let t: TokenResp = resp.json().await.wrap_err("decoding the token response")?;
		*cached = Some(t.access_token.clone());
		Ok(t.access_token)
	}
}

/// Scans a Business Profile: the API's list, with screenshots matched from the Maps page.
#[derive(Debug)]
pub struct GbpSource<'a> {
	/// The API.
	pub client: &'a Client,
	/// Where screenshots are taken.
	pub browser: &'a Browser,
	/// The screenshot walk's limits.
	pub defaults: &'a Defaults,
}

impl ReviewSource for GbpSource<'_> {
	async fn scan(&self, target: &Target, known: &Known) -> eyre::Result<Scan> {
		let loc = target.gbp.as_ref().ok_or_else(|| eyre::eyre!("target {} is gbp but has no account/location", target.id))?;
		let list = self.client.reviews(loc).await?;
		let mut reviews: Vec<Observed> = list.reviews.into_iter().map(observed).collect();
		let mut warnings = Vec::new();
		let coverage = list_coverage(reviews.len(), list.total, &mut warnings);

		let wanted: Vec<&Observed> = reviews.iter().filter(|r| known.wants_capture(&r.source_review_id)).collect();
		if !wanted.is_empty() {
			let max = self.defaults.max_for(known);
			let mut policy = FindCards::new(&wanted, Timestamp::now());
			// The API list stands on its own; a failed screenshot pass only leaves captures pending.
			match self.browser.walk(&target.place_id, &target.lang, &mut policy, max).await {
				Ok(walked) => {
					warnings.extend(walked.warnings);
					if walked.end.cut_short() && !policy.satisfied() {
						warnings.push("the Maps walk stopped before matching every new API review; they stay without a screenshot".to_owned());
					}
					let mut matched = match_captures(&reviews, |id| known.wants_capture(id), walked.cards);
					for r in &mut reviews {
						r.capture = matched.remove(&r.source_review_id);
					}
				}
				Err(e) => warnings.push(format!("screenshots skipped, Maps page failed: {e:#}")),
			}
		}
		Ok(Scan {
			reviews,
			coverage,
			warnings,
			cut_after: None,
		})
	}
}
