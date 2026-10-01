//! The page-driving selectors on saved pages: each list resolves the way the in-page
//! `CLICK_FIRST` does — the first selector with a match wins, and its first match is the one.

use review_archive_core::maps::selectors as sel;
use scraper::{ElementRef, Html, Selector};

fn fixture(name: &str) -> Html {
	let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
	Html::parse_document(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}

fn first<'a>(doc: &'a Html, sels: &[&str]) -> Option<ElementRef<'a>> {
	sels.iter()
		// `:has()` is for the browser; scraper does not implement it
		.filter(|s| !s.contains(":has("))
		.find_map(|s| doc.select(&Selector::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))).next())
}

fn text(el: ElementRef<'_>) -> String {
	el.text().collect::<String>().trim().to_owned()
}

#[test]
fn signed_out_sort_opens_the_sign_in_dialog() {
	let doc = fixture("maps_en_sort_gated.html");

	let sort = first(&doc, sel::SORT_BUTTON).expect("the sort button");
	assert_eq!(sort.value().attr("aria-label"), Some("Sort reviews"));

	// the topic chips carry data-index="1" too; they are not the menu's "newest"
	assert!(doc.select(&Selector::parse(r#"[role="radio"][data-index="1"]"#).unwrap()).next().is_some());
	assert!(first(&doc, sel::SORT_NEWEST).is_none(), "no sort menu on this page");

	let gate = first(&doc, sel::SIGN_IN_GATE).expect("the sign-in dialog's button");
	assert_eq!(text(gate), "Sign in");
	let dismiss = first(&doc, sel::PROMO_DISMISS).expect("the dialog's dismiss button");
	assert_eq!(text(dismiss), "Dismiss");
}

#[test]
fn a_signed_out_page_says_so_in_its_account_corner() {
	let doc = fixture("maps_en_signed_out_header.html");
	assert_eq!(first(&doc, sel::SIGNED_OUT).and_then(|a| a.value().attr("aria-label")), Some("Sign in"));
	assert!(first(&fixture("maps_en_replies.html"), sel::SIGNED_OUT).is_none(), "a review list alone is no sign of either");
}

#[test]
fn limited_view_has_a_place_but_no_reviews_tab() {
	let doc = fixture("maps_fr_limited_view.html");

	assert!(first(&doc, sel::REVIEWS_TAB).is_none(), "the limited view has no reviews tab");
	assert_eq!(first(&doc, sel::PLACE_TITLE).map(text).as_deref(), Some("Tour Eiffel"));
	// as `js::HAS_TEXT` reads it: the page text, non-breaking spaces as spaces
	let body = doc.root_element().text().collect::<String>().replace('\u{a0}', " ");
	assert!(sel::LIMITED_VIEW_TEXT.iter().any(|t| body.contains(t)), "the limited view's notice");
}
