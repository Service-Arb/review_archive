//! Review cards out of Maps HTML. Pure: the browser hands over markup, this reads it.

use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;

use super::selectors as sel;

/// One card as the page renders it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Card {
	/// `data-review-id`: Google's id for the review.
	pub id: String,
	/// The reviewer's display name.
	pub author: String,
	/// The reviewer's `/maps/contrib/` profile, query stripped.
	pub author_url: Option<String>,
	/// 1–5 stars.
	pub rating: Option<u8>,
	/// "3 weeks ago", in the page's language.
	pub date_raw: Option<String>,
	/// The review text, line breaks kept; `None` for a rating without words.
	pub text: Option<String>,
	/// The owner's response.
	pub reply: Option<String>,
	/// Photos attached, the "+ 5" tile counted as five.
	pub photo_count: u32,
}

/// Parses every outermost review card in `html`, in document order.
pub fn cards(html: &str) -> Vec<Card> {
	let doc = Html::parse_fragment(html);
	let card_sel = selector(sel::CARD);
	doc.select(&card_sel)
		.filter(|el| !el.ancestors().filter_map(ElementRef::wrap).any(|a| a.value().attr(sel::CARD_ID_ATTR).is_some()))
		.filter_map(card)
		.collect()
}

fn card(el: ElementRef<'_>) -> Option<Card> {
	let id = el.value().attr(sel::CARD_ID_ATTR)?.trim().to_owned();
	if id.is_empty() {
		return None;
	}
	let reply_block = first(el, sel::REPLY_BLOCK);
	let outside_reply = |e: &ElementRef<'_>| reply_block.is_none_or(|r| !e.ancestors().any(|a| a.id() == r.id()) && e.id() != r.id());

	let author = first_text(el, sel::AUTHOR_NAME)
		.or_else(|| el.value().attr("aria-label").map(|s| s.trim().to_owned()))
		.unwrap_or_default();
	let author_url = sel::AUTHOR_LINK
		.iter()
		.flat_map(|s| el.select(&selector(s)).collect::<Vec<_>>())
		.find_map(|a| a.value().attr("data-href").or_else(|| a.value().attr("href")))
		.map(strip_query);

	let rating = sel::RATING_STARS
		.iter()
		.flat_map(|s| el.select(&selector(s)).collect::<Vec<_>>())
		.filter(outside_reply)
		.find_map(|e| e.value().attr("aria-label").and_then(leading_rating))
		.or_else(|| first_text(el, sel::RATING_TEXT).as_deref().and_then(leading_rating));

	let date_raw = sel::DATE
		.iter()
		.flat_map(|s| el.select(&selector(s)).collect::<Vec<_>>())
		.find(outside_reply)
		.map(text_of)
		.filter(|s| !s.is_empty());

	let text = sel::BODY_TEXT
		.iter()
		.flat_map(|s| el.select(&selector(s)).collect::<Vec<_>>())
		.find(outside_reply)
		.map(text_of)
		.filter(|s| !s.is_empty());

	let reply = reply_block.and_then(|r| first_text(r, sel::REPLY_TEXT));

	let tiles = sel::PHOTO.iter().map(|s| el.select(&selector(s)).count()).find(|&n| n > 0).unwrap_or(0);
	let more = first(el, sel::PHOTO_MORE).and_then(|m| first_text(m, sel::PHOTO_MORE_COUNT)).and_then(|t| leading_number(&t));
	let photo_count = match more {
		Some(n) if tiles > 0 => tiles - 1 + n,
		_ => tiles,
	};

	Some(Card {
		id,
		author,
		author_url,
		rating,
		date_raw,
		text,
		reply,
		photo_count: u32::try_from(photo_count).unwrap_or(u32::MAX),
	})
}

/// How many reviews the list holds, from the `aria-label`s of its histogram rows
/// ([`sel::HISTOGRAM_ROW`]): one row per star, each "<stars> <word>, <count> <word>".
/// `None` unless there are exactly five rows and every one reads.
pub fn review_total(row_labels: &[String]) -> Option<u64> {
	if row_labels.len() != 5 {
		return None;
	}
	row_labels
		.iter()
		.map(|l| {
			// the count follows the first comma and may be grouped with ",", ".", " " or a nbsp
			let (_, count) = l.split_once(',')?;
			let digits: String = count.chars().filter(char::is_ascii_digit).collect();
			digits.parse::<u64>().ok()
		})
		.sum()
}

fn selector(s: &str) -> Selector {
	// Selectors are compile-time constants in `selectors.rs`; the parser tests parse every one.
	Selector::parse(s).unwrap_or_else(|e| panic!("bad selector {s:?} in selectors.rs: {e}"))
}

fn first<'a>(el: ElementRef<'a>, sels: &[&str]) -> Option<ElementRef<'a>> {
	sels.iter().find_map(|s| el.select(&selector(s)).next())
}

fn first_text(el: ElementRef<'_>, sels: &[&str]) -> Option<String> {
	sels.iter().flat_map(|s| el.select(&selector(s)).collect::<Vec<_>>()).map(text_of).find(|t| !t.is_empty())
}

/// Visible text with `<br>` as line breaks and whitespace runs within a line collapsed.
fn text_of(el: ElementRef<'_>) -> String {
	let mut raw = String::new();
	for node in el.descendants() {
		match node.value() {
			Node::Text(t) => raw.push_str(t),
			Node::Element(e) if e.name() == "br" => raw.push('\n'),
			_ => {}
		}
	}
	raw.lines()
		.map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
		.collect::<Vec<_>>()
		.join("\n")
		.trim()
		.to_owned()
}

fn leading_number(s: &str) -> Option<usize> {
	let start = s.find(|c: char| c.is_ascii_digit())?;
	s[start..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// The first number in "5 stars", "Rated 4.0 out of 5", "4/5", "5 étoiles" — if it is a valid rating.
fn leading_rating(s: &str) -> Option<u8> {
	leading_number(s).and_then(|n| u8::try_from(n).ok()).filter(|r| (1..=5).contains(r))
}

fn strip_query(url: &str) -> String {
	url.split(['?', '#']).next().unwrap_or(url).to_owned()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_selector_parses() {
		let lists = [
			sel::CONSENT_REJECT,
			sel::PROMO_DISMISS,
			sel::REVIEWS_TAB,
			sel::SORT_BUTTON,
			sel::SORT_NEWEST,
			sel::EXPAND,
			sel::AUTHOR_NAME,
			sel::AUTHOR_LINK,
			sel::RATING_STARS,
			sel::RATING_TEXT,
			sel::DATE,
			sel::REPLY_BLOCK,
			sel::BODY_TEXT,
			sel::REPLY_TEXT,
			sel::PHOTO,
			sel::PHOTO_MORE,
			sel::PHOTO_MORE_COUNT,
		];
		for s in lists.iter().flat_map(|l| l.iter()).chain([&sel::CARD, &sel::js::MARKED_CARD, &sel::HISTOGRAM_ROW]) {
			// `:has()` is for the browser; scraper does not implement it
			if !s.contains(":has(") {
				selector(s);
			}
		}
	}

	#[test]
	fn ratings() {
		assert_eq!(leading_rating("5 stars"), Some(5));
		assert_eq!(leading_rating("1 étoile"), Some(1));
		assert_eq!(leading_rating("Rated 4.0 out of 5,"), Some(4));
		assert_eq!(leading_rating("4/5"), Some(4));
		assert_eq!(leading_rating("12 photos"), None);
		assert_eq!(leading_rating("stars"), None);
	}

	#[test]
	fn totals_from_histogram_labels() {
		let en: Vec<String> = [
			"5 stars, 4,377 reviews",
			"4 stars, 1,892 reviews",
			"3 stars, 976 reviews",
			"2 stars, 443 reviews",
			"1 stars, 12 reviews",
		]
		.map(String::from)
		.into();
		assert_eq!(review_total(&en), Some(4377 + 1892 + 976 + 443 + 12));
		let fr: Vec<String> = [
			"5\u{a0}étoiles, 396 164\u{a0}avis",
			"4\u{a0}étoiles, 63 370\u{a0}avis",
			"3\u{a0}étoiles, 0\u{a0}avis",
			"2\u{a0}étoiles, 1\u{a0}avis",
			"1\u{a0}étoile, 9 880\u{a0}avis",
		]
		.map(String::from)
		.into();
		assert_eq!(review_total(&fr), Some(396_164 + 63_370 + 1 + 9_880));
		assert_eq!(review_total(&en[..4]), None, "a row short is not a total");
		assert_eq!(review_total(&["5 stars"; 5].map(String::from)), None);
	}

	#[test]
	fn nested_ids_are_one_card() {
		let html = r#"<div data-review-id="A" aria-label="Ann"><button data-review-id="A">Like</button><span class="rsqaWe">a week ago</span></div>"#;
		let got = cards(html);
		assert_eq!(got.len(), 1);
		assert_eq!(got[0].author, "Ann");
		assert_eq!(got[0].date_raw.as_deref(), Some("a week ago"));
	}
}
