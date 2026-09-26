//! `gbp`: profiles we manage, from the official Business Profile API. The list is
//! authoritative; the screenshots still come from the public Maps page.
//!
//! Read-only: the only calls made are the OAuth token refresh and `reviews.list`.

use std::collections::{HashMap, hash_map::Entry};

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
	domain::{Capture, Coverage, GbpLocation, Known, Observed, ReviewSource, Scan, Target, normalise_ws, relative_date},
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
	/// Absent, not empty, on a location without reviews: the API leaves out empty lists.
	#[serde(default)]
	reviews: Vec<ApiReview>,
	total_review_count: Option<u64>,
	next_page_token: Option<String>,
}

/// A location's reviews, all pages, and the count the API says there are.
#[derive(Clone, Debug)]
pub struct ReviewList {
	pub reviews: Vec<ApiReview>,
	pub total: Option<u64>,
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
	pub async fn reviews(&self, loc: &GbpLocation) -> eyre::Result<ReviewList> {
		let url = format!("{}/v4/accounts/{}/locations/{}/reviews", self.api_base, loc.account, loc.location);
		let mut out = ReviewList { reviews: Vec::new(), total: None };
		let mut page_token: Option<String> = None;
		loop {
			let page: ReviewsPage = self.get(&url, page_token.as_deref()).await?;
			out.reviews.extend(page.reviews);
			out.total = out.total.or(page.total_review_count);
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
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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

	fn of_observed(r: &Observed) -> Self {
		Self::new(&r.author, r.rating, r.text.as_deref())
	}
}

/// Walks the Maps list until every wanted card is found, or the walk is past them all.
struct FindCards {
	/// Wanted and not yet seen, with when the API says each was posted.
	remaining: HashMap<MatchKey, Option<Timestamp>>,
	/// The latest the last card read can have been posted.
	last_card_at: Option<Timestamp>,
	now: Timestamp,
}

impl FindCards {
	fn new(wanted: &[&Observed], now: Timestamp) -> Self {
		let mut remaining = HashMap::with_capacity(wanted.len());
		for r in wanted {
			remaining.insert(MatchKey::of_observed(r), r.published_est);
		}
		Self { remaining, last_card_at: None, now }
	}
}

impl WalkPolicy for FindCards {
	fn wants_capture(&self, card: &Card) -> bool {
		self.remaining.contains_key(&MatchKey::of_card(card))
	}

	fn observe(&mut self, card: &Card) {
		self.remaining.remove(&MatchKey::of_card(card));
		if let Some(at) = card.date_raw.as_deref().and_then(|d| relative_date::estimate(d, self.now)) {
			self.last_card_at = Some(at);
		}
	}

	/// Every wanted card found — or the list, newest first, has gone below the time every
	/// remaining one was posted at, so they were passed without a match (a text Maps shows
	/// differently, say). Those stay `capture_pending` for the next scan.
	fn satisfied(&self) -> bool {
		let Some(last) = self.last_card_at else {
			return self.remaining.is_empty();
		};
		self.remaining.values().all(|posted| posted.is_some_and(|p| p > last))
	}
}

/// Pairs screenshots with the API reviews that want one. A key that two API reviews, or
/// two cards, share is ambiguous: the card could be either, so neither gets it and both
/// stay pending.
pub fn match_captures(api: &[Observed], wants: impl Fn(&str) -> bool, cards: Vec<(Card, Option<Capture>)>) -> HashMap<String, Capture> {
	let mut by_key: HashMap<MatchKey, Option<&str>> = HashMap::with_capacity(api.len());
	for r in api {
		match by_key.entry(MatchKey::of_observed(r)) {
			Entry::Vacant(e) => {
				e.insert(Some(r.source_review_id.as_str()));
			}
			Entry::Occupied(mut e) => {
				e.insert(None);
			}
		}
	}
	let mut card_counts: HashMap<MatchKey, usize> = HashMap::with_capacity(cards.len());
	for (card, _) in &cards {
		*card_counts.entry(MatchKey::of_card(card)).or_default() += 1;
	}
	let mut out = HashMap::new();
	for (card, capture) in cards {
		let Some(capture) = capture else { continue };
		let key = MatchKey::of_card(&card);
		if card_counts.get(&key) != Some(&1) {
			continue;
		}
		if let Some(Some(id)) = by_key.get(&key)
			&& wants(id)
		{
			out.insert((*id).to_owned(), capture);
		}
	}
	out
}

/// How much of the location the API list is known to hold.
fn list_coverage(listed: usize, total: Option<u64>, warnings: &mut Vec<String>) -> Coverage {
	let listed = u64::try_from(listed).unwrap_or(u64::MAX);
	match total {
		Some(total) if listed < total => {
			warnings.push(format!("the API listed {listed} reviews of the {total} it counts; no review is judged gone this run"));
			Coverage::DownTo(None)
		}
		_ => Coverage::Complete,
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
		let list = self.client.reviews(loc).await?;
		let mut reviews: Vec<Observed> = list.reviews.into_iter().map(observed).collect();
		let mut warnings = Vec::new();
		let coverage = list_coverage(reviews.len(), list.total, &mut warnings);

		let wanted: Vec<&Observed> = reviews.iter().filter(|r| known.wants_capture(&r.source_review_id)).collect();
		if !wanted.is_empty() {
			let max = if known.is_empty() {
				self.defaults.max_reviews_initial
			} else {
				self.defaults.max_reviews_per_scan
			};
			let mut policy = FindCards::new(&wanted, Timestamp::now());
			// The API list stands on its own; a failed screenshot pass only leaves captures pending.
			match self.browser.walk(&target.place_id, &target.lang, &mut policy, max).await {
				Ok(walked) => {
					if walked.end == WalkEnd::Cap {
						tracing::info!(target = %target.id, "Maps walk hit its cap before matching every API review");
					}
					warnings.extend(walked.warnings);
					let mut matched = match_captures(&reviews, |id| known.wants_capture(id), walked.cards);
					for r in &mut reviews {
						r.capture = matched.remove(&r.source_review_id);
					}
				}
				Err(e) => warnings.push(format!("screenshots skipped, Maps page failed: {e:#}")),
			}
		}
		Ok(Scan { reviews, coverage, warnings })
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

	fn api(id: &str, author: &str, rating: u8, text: Option<&str>) -> Observed {
		Observed {
			source_review_id: id.into(),
			author: author.into(),
			rating: Some(rating),
			text: text.map(Into::into),
			..Default::default()
		}
	}

	fn shot(card_id: &str, author: &str, rating: u8, text: Option<&str>) -> (Card, Option<Capture>) {
		(
			Card {
				id: card_id.into(),
				author: author.into(),
				rating: Some(rating),
				text: text.map(Into::into),
				..Default::default()
			},
			Some(Capture {
				png: card_id.as_bytes().to_vec(),
				captured_at: "2026-09-26T12:00:00Z".parse().unwrap(),
				page_url: String::new(),
			}),
		)
	}

	#[test]
	fn ambiguous_keys_attach_nothing() {
		// two rating-only five-star reviews by namesakes: a card of either could be the other's
		let reviews = [
			api("r1", "Jean", 5, None),
			api("r2", "Jean", 5, None),
			api("r3", "Ann", 4, Some("Nice")),
			api("r4", "Bob", 1, Some("Bad")),
		];
		let cards = vec![
			shot("c1", "Jean", 5, None),
			shot("c3", "Ann", 4, Some("Nice")),
			// two cards read the same: which one is r4's is unknown
			shot("c4a", "Bob", 1, Some("Bad")),
			shot("c4b", "Bob", 1, Some("Bad")),
		];
		let got = match_captures(&reviews, |_| true, cards);
		assert_eq!(got.keys().map(String::as_str).collect::<Vec<_>>(), ["r3"]);
		assert_eq!(got["r3"].png, b"c3");
	}

	#[test]
	fn only_wanted_reviews_get_a_capture() {
		let reviews = [api("r1", "Ann", 4, Some("Nice")), api("r2", "Bob", 2, Some("Meh"))];
		let cards = vec![shot("c1", "Ann", 4, Some("Nice")), shot("c2", "Bob", 2, Some("Meh"))];
		let got = match_captures(&reviews, |id| id == "r2", cards);
		assert_eq!(got.keys().map(String::as_str).collect::<Vec<_>>(), ["r2"]);
	}

	#[test]
	fn the_walk_stops_once_past_every_remaining_review() {
		let now: Timestamp = "2026-09-26T12:00:00Z".parse().unwrap();
		let mut found = api("r1", "Ann", 4, Some("Nice"));
		found.published_est = Some("2026-09-20T00:00:00Z".parse().unwrap());
		// Maps shows this one translated, so it never matches
		let mut unmatchable = api("r2", "Bob", 2, Some("Meh"));
		unmatchable.published_est = Some("2026-09-10T00:00:00Z".parse().unwrap());
		let mut policy = FindCards::new(&[&found, &unmatchable], now);
		let card = |author: &str, rating: u8, text: &str, date: &str| Card {
			author: author.into(),
			rating: Some(rating),
			text: Some(text.into()),
			date_raw: Some(date.into()),
			..Default::default()
		};

		policy.observe(&card("Ann", 4, "Nice", "6 days ago"));
		assert!(!policy.satisfied());
		// a card from 2026-09-12 at the latest: Bob's (2026-09-10) may still be below
		policy.observe(&card("Cy", 5, "x", "2 weeks ago"));
		assert!(!policy.satisfied());
		// from 2026-08-26 at the latest: Bob's would have been above it
		policy.observe(&card("Dee", 5, "y", "a month ago"));
		assert!(policy.satisfied());
	}

	#[test]
	fn a_short_api_list_is_not_complete() {
		let mut w = Vec::new();
		assert_eq!(list_coverage(3, Some(3), &mut w), Coverage::Complete);
		assert_eq!(list_coverage(3, None, &mut w), Coverage::Complete);
		assert!(w.is_empty());
		// `reviews` missing from a response that counts three
		assert_eq!(list_coverage(0, Some(3), &mut w), Coverage::DownTo(None));
		assert_eq!(w.len(), 1);
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
