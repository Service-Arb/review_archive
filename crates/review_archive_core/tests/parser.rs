//! The Maps parser on saved pages. When Google changes the markup: refresh the fixtures
//! with `review_archive scan <id> --dump-html <dir>`, fix `src/maps/selectors.rs`,
//! and `cargo insta review`.

use review_archive_core::maps::parse::{self, Card};

fn fixture(name: &str) -> Vec<Card> {
	let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
	parse::cards(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}

/// What must hold on any live page, whatever the snapshot says.
fn invariants(cards: &[Card], expected: usize) {
	assert_eq!(cards.len(), expected, "one card per review, nested ids not double-counted");
	for c in cards {
		assert!(!c.id.is_empty() && !c.author.is_empty(), "{c:?}");
		assert!(c.rating.is_some_and(|r| (1..=5).contains(&r)), "{c:?}");
		assert!(c.date_raw.is_some(), "{c:?}");
		assert!(c.author_url.as_deref().is_some_and(|u| u.contains("/maps/contrib/") && !u.contains('?')), "{c:?}");
	}
}

#[test]
fn fr_photos() {
	let cards = fixture("maps_fr_photos.html");
	invariants(&cards, 5);
	// three tiles and a "+ 5" tile
	assert_eq!(cards[0].photo_count, 8);
	assert!(cards[0].text.as_deref().unwrap().contains('\n'), "line breaks survive");
	insta::assert_yaml_snapshot!(cards);
}

#[test]
fn fr_replies() {
	let cards = fixture("maps_fr_replies.html");
	invariants(&cards, 6);
	assert!(cards.iter().any(|c| c.reply.is_some()));
	for c in &cards {
		if let (Some(text), Some(reply)) = (&c.text, &c.reply) {
			assert!(!text.contains(reply.as_str()), "the reply is not part of the review text: {c:?}");
		}
	}
	insta::assert_yaml_snapshot!(cards);
}

#[test]
fn en_replies() {
	let cards = fixture("maps_en_replies.html");
	invariants(&cards, 6);
	assert!(cards.iter().any(|c| c.reply.is_some()));
	insta::assert_yaml_snapshot!(cards);
}
