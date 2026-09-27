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
