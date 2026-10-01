//! Business Profile reviews without the HTTP: the API's JSON, and how an API review is
//! matched to its card on the public Maps page (where the screenshot comes from).

use std::collections::{HashMap, hash_map::Entry};

use jiff::Timestamp;
use serde::Deserialize;

use crate::{
	Capture, Coverage, Observed,
	maps::{Card, WalkPolicy, WalkedCard},
	normalise_ws, relative_date,
};

/// Characters of review text compared when matching an API review to a Maps card.
const MATCH_PREFIX: usize = 40;

/// One page of `accounts/{a}/locations/{l}/reviews`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewsPage {
	/// Absent, not empty, on a location without reviews: the API leaves out empty lists.
	#[serde(default)]
	pub reviews: Vec<ApiReview>,
	/// How many reviews the location has.
	pub total_review_count: Option<u64>,
	/// Set while there are more pages.
	pub next_page_token: Option<String>,
}

/// A location's reviews, all pages, and the count the API says there are.
#[derive(Clone, Debug)]
pub struct ReviewList {
	/// Every review listed.
	pub reviews: Vec<ApiReview>,
	/// `totalReviewCount`, when the API gave it.
	pub total: Option<u64>,
}

/// A review as the Business Profile API returns it.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiReview {
	/// The API's id for it.
	pub review_id: String,
	/// Who wrote it.
	#[serde(default)]
	pub reviewer: Reviewer,
	/// `ONE` … `FIVE`.
	pub star_rating: Option<String>,
	/// The text.
	pub comment: Option<String>,
	/// RFC 3339.
	pub create_time: Option<String>,
	/// RFC 3339.
	pub update_time: Option<String>,
	/// The owner's response.
	pub review_reply: Option<Reply>,
}

/// The reviewer of an [`ApiReview`].
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reviewer {
	/// As shown publicly.
	pub display_name: Option<String>,
}

/// The owner's response to an [`ApiReview`].
#[derive(Clone, Debug, Deserialize)]
pub struct Reply {
	/// The text.
	pub comment: Option<String>,
}

/// `FIVE` → 5.
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

/// An API review as an observation.
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

/// How much of the location the API list is known to hold: all of it, unless the API
/// listed fewer than it counts.
pub fn list_coverage(listed: usize, total: Option<u64>, warnings: &mut Vec<String>) -> Coverage {
	let listed = u64::try_from(listed).unwrap_or(u64::MAX);
	match total {
		Some(total) if listed < total => {
			warnings.push(format!("the API listed {listed} reviews of the {total} it counts; no review is judged gone this run"));
			Coverage::DownTo(None)
		}
		_ => Coverage::Complete,
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
	/// Case and whitespace do not count; only the first 40 characters of text do.
	pub fn new(author: &str, rating: Option<u8>, text: Option<&str>) -> Self {
		Self {
			author: normalise_ws(author).to_lowercase(),
			rating,
			prefix: normalise_ws(text.unwrap_or_default()).to_lowercase().chars().take(MATCH_PREFIX).collect(),
		}
	}

	/// The key of a Maps card.
	pub fn of_card(c: &Card) -> Self {
		Self::new(&c.author, c.rating, c.text.as_deref())
	}

	/// The key of an observation.
	pub fn of_observed(r: &Observed) -> Self {
		Self::new(&r.author, r.rating, r.text.as_deref())
	}
}

/// Walks the Maps list until every wanted card is found, or the walk is past them all.
#[derive(Debug)]
pub struct FindCards {
	/// Wanted and not yet seen, with when the API says each was posted.
	remaining: HashMap<MatchKey, Option<Timestamp>>,
	/// The latest the last card read can have been posted.
	last_card_at: Option<Timestamp>,
	now: Timestamp,
}

impl FindCards {
	/// Looks for the cards of these reviews; `now` dates the cards' relative dates.
	pub fn new(wanted: &[&Observed], now: Timestamp) -> Self {
		let mut remaining = HashMap::with_capacity(wanted.len());
		for r in wanted {
			remaining.insert(MatchKey::of_observed(r), r.published_est);
		}
		Self { remaining, last_card_at: None, now }
	}
}

impl WalkPolicy for FindCards {
	fn wants_list(&self, _: Option<u64>) -> bool {
		true
	}

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

/// Pairs screenshots with the API reviews that want one, by source review id. A key that
/// two API reviews, or two cards, share is ambiguous: the card could be either, so neither
/// gets it and both stay pending.
pub fn match_captures(api: &[Observed], wants: impl Fn(&str) -> bool, cards: Vec<WalkedCard>) -> HashMap<String, Capture> {
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

	fn shot(card_id: &str, author: &str, rating: u8, text: Option<&str>) -> WalkedCard {
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
