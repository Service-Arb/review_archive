//! `gbp`: profiles we manage, from the official Business Profile API. The list is
//! authoritative; the screenshots still come from the public Maps page.
//!
//! Read-only: the only calls made are the OAuth token refresh and `reviews.list`.

use eyre::WrapErr;
use jiff::Timestamp;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::maps::{
	Browser,
	browser::{WalkEnd, WalkPolicy},
	parse::Card,
};
use crate::{
	config::Defaults,
	domain::{Capture, Coverage, GbpLocation, Known, Observed, ReviewSource, Scan, Target, normalise_ws},
};

pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const API_BASE: &str = "https://mybusiness.googleapis.com";
const PAGE_SIZE: u32 = 50;
/// Characters of review text compared when matching an API review to a Maps card.
const MATCH_PREFIX: usize = 40;

#[derive(Clone, Debug)]
pub struct Credentials {
	pub client_id: String,
	pub client_secret: String,
	pub refresh_token: String,
}

impl Credentials {
	pub fn from_env() -> eyre::Result<Self> {
		let var = |k: &str| std::env::var(k).wrap_err_with(|| format!("{k} is not set (needed for gbp targets)"));
		Ok(Self {
			client_id: var("GBP_CLIENT_ID")?,
			client_secret: var("GBP_CLIENT_SECRET")?,
			refresh_token: var("GBP_REFRESH_TOKEN")?,
		})
	}
}

pub struct Client {
	http: reqwest::Client,
	creds: Credentials,
	token_url: String,
	api_base: String,
	token: Mutex<Option<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewsPage {
	#[serde(default)]
	reviews: Vec<ApiReview>,
	next_page_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiReview {
	pub review_id: String,
	#[serde(default)]
	pub reviewer: Reviewer,
	pub star_rating: Option<String>,
	pub comment: Option<String>,
	pub create_time: Option<String>,
	pub update_time: Option<String>,
	pub review_reply: Option<Reply>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reviewer {
	pub display_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Reply {
	pub comment: Option<String>,
}

impl Client {
	pub fn new(http: reqwest::Client, creds: Credentials) -> Self {
		Self::with_endpoints(http, creds, TOKEN_URL, API_BASE)
	}

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
	pub async fn reviews(&self, loc: &GbpLocation) -> eyre::Result<Vec<ApiReview>> {
		let url = format!("{}/v4/accounts/{}/locations/{}/reviews", self.api_base, loc.account, loc.location);
		let mut out = Vec::new();
		let mut page_token: Option<String> = None;
		loop {
			let page: ReviewsPage = self.get(&url, page_token.as_deref()).await?;
			out.extend(page.reviews);
			match page.next_page_token.filter(|t| !t.is_empty()) {
				Some(t) => page_token = Some(t),
				None => break,
			}
		}
		Ok(out)
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

pub fn star_rating(s: &str) -> Option<u8> {
	match s {
		"ONE" => Some(1),
		"TWO" => Some(2),
		"THREE" => Some(3),
		"FOUR" => Some(4),
		"FIVE" => Some(5),
		_ => None,
	}
}

pub fn observed(r: ApiReview) -> Observed {
	Observed {
		source_review_id: r.review_id,
		author: r.reviewer.display_name.unwrap_or_default(),
		author_url: None,
		rating: r.star_rating.as_deref().and_then(star_rating),
		text: r.comment.filter(|c| !c.trim().is_empty()),
		reply: r.review_reply.and_then(|r| r.comment).filter(|c| !c.trim().is_empty()),
		photo_count: 0,
		published_est: r.create_time.as_deref().and_then(|t| t.parse::<Timestamp>().ok()),
		published_raw: r.create_time,
		capture: None,
	}
}

/// What a review and a Maps card are compared on: author, rating, the start of the text.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchKey {
	author: String,
	rating: Option<u8>,
	prefix: String,
}

impl MatchKey {
	pub fn new(author: &str, rating: Option<u8>, text: Option<&str>) -> Self {
		Self {
			author: normalise_ws(author).to_lowercase(),
			rating,
			prefix: normalise_ws(text.unwrap_or_default()).to_lowercase().chars().take(MATCH_PREFIX).collect(),
		}
	}

	fn of_card(c: &Card) -> Self {
		Self::new(&c.author, c.rating, c.text.as_deref())
	}
}

struct FindCards<'a> {
	wanted: &'a [MatchKey],
}

impl WalkPolicy for FindCards<'_> {
	fn is_known(&self, _: &Card) -> bool {
		false
	}

	fn wants_capture(&self, card: &Card) -> bool {
		self.wanted.contains(&MatchKey::of_card(card))
	}

	fn satisfied(&self, cards: &[Card]) -> bool {
		let found: Vec<MatchKey> = cards.iter().map(MatchKey::of_card).collect();
		self.wanted.iter().all(|w| found.contains(w))
	}
}

pub struct GbpSource<'a> {
	pub client: &'a Client,
	pub browser: &'a Browser,
	pub defaults: &'a Defaults,
}

impl ReviewSource for GbpSource<'_> {
	async fn scan(&self, target: &Target, known: &Known) -> eyre::Result<Scan> {
		let loc = target.gbp.as_ref().ok_or_else(|| eyre::eyre!("target {} is gbp but has no account/location", target.id))?;
		let mut reviews: Vec<Observed> = self.client.reviews(loc).await?.into_iter().map(observed).collect();
		let mut warnings = Vec::new();

		let wanted: Vec<MatchKey> = reviews
			.iter()
			.filter(|r| known.wants_capture(&r.source_review_id))
			.map(|r| MatchKey::new(&r.author, r.rating, r.text.as_deref()))
			.collect();
		if !wanted.is_empty() {
			let max = if known.is_empty() {
				self.defaults.max_reviews_initial
			} else {
				self.defaults.max_reviews_per_scan
			};
			// The API list stands on its own; a failed screenshot pass only leaves captures pending.
			match self.browser.walk(&target.place_id, &target.lang, &FindCards { wanted: &wanted }, max).await {
				Ok(walked) => {
					let found: Vec<(MatchKey, Capture)> = walked.cards.into_iter().filter_map(|(card, capture)| Some((MatchKey::of_card(&card), capture?))).collect();
					for r in reviews.iter_mut().filter(|r| known.wants_capture(&r.source_review_id)) {
						let key = MatchKey::new(&r.author, r.rating, r.text.as_deref());
						r.capture = found.iter().find(|(k, _)| *k == key).map(|(_, c)| c.clone());
					}
					warnings.extend(walked.warnings);
					if walked.end == WalkEnd::Cap {
						tracing::info!(target = %target.id, "Maps walk hit its cap before matching every API review");
					}
				}
				Err(e) => warnings.push(format!("screenshots skipped, Maps page failed: {e:#}")),
			}
		}
		Ok(Scan {
			reviews,
			coverage: Coverage::Complete,
			warnings,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keys_match_across_whitespace_and_case() {
		let api = MatchKey::new("Marie  D.", Some(5), Some("Super café,\n très bon accueil et un service rapide, je recommande vivement"));
		let card = Card {
			author: "marie d.".into(),
			rating: Some(5),
			text: Some("Super café, très bon accueil et un service rapide, je recommande".into()),
			..Default::default()
		};
		assert_eq!(api, MatchKey::of_card(&card));
		assert_ne!(api, MatchKey::new("Marie D.", Some(4), Some("Super café, très bon accueil")));
	}

	#[test]
	fn api_review_to_observed() {
		let r: ApiReview = serde_json::from_value(serde_json::json!({
			"reviewId": "r1",
			"reviewer": { "displayName": "Ann" },
			"starRating": "FOUR",
			"comment": "Nice",
			"createTime": "2026-09-01T10:00:00.123Z",
			"reviewReply": { "comment": "Thanks" }
		}))
		.unwrap();
		let o = observed(r);
		assert_eq!(o.rating, Some(4));
		assert_eq!(o.reply.as_deref(), Some("Thanks"));
		assert_eq!(o.published_est.unwrap().to_string(), "2026-09-01T10:00:00.123Z");
	}
}
