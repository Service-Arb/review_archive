//! Every assumption about Google Maps' markup, in one place.
//!
//! The page changes without notice. When a scan starts failing or fields come back
//! empty, this file is the fix: refresh the fixtures with `scan --dump-html <dir>`,
//! adjust the selectors here, and `cargo insta review` the parser snapshots.
//!
//! Where a selector can rest on something stable — an ARIA role, a `data-*` attribute,
//! a `jsaction` name — it does; the obfuscated class names are fallbacks. Each list is
//! tried in order and the first that matches wins.

/// Where a place's page lives. `hl` sets the UI language, which decides the language
/// of the relative dates the parser reads.
pub fn place_url(place_id: &str, lang: &str) -> String {
	format!("https://www.google.com/maps/place/?q=place_id:{place_id}&hl={lang}")
}

/// The EU consent interstitial is served from this host.
pub const CONSENT_HOST: &str = "consent.google.com";
/// Google's "unusual traffic" page. Hitting it fails the run; nothing tries to get past it.
pub const BLOCKED_PATH: &str = "/sorry/";

/// The notice of Google's "limited view" of Maps, which has no reviews at all. Seen 2026-09
/// for headless Chrome, and for a windowed Chrome on a fresh profile; a windowed Chrome on a
/// profile that had been served the full page before kept getting it. Nothing here tries to
/// change Google's mind — the run fails and says why. Matched against the page text: the
/// notice carries no stable attribute.
pub const LIMITED_VIEW_TEXT: &[&str] = &[
	"limited view of Google Maps",
	"affichage limité de Google",
	"eingeschränkte Ansicht von Google",
	"vista limitada de Google",
	"visualizzazione limitata di Google",
];

/// "Not now" on the "sign in to get the most out of Maps" dialog a fresh profile is shown
/// over the review list; it swallows clicks until dismissed. Dismissing is all this does.
pub const PROMO_DISMISS: &[&str] = &[r#"[role="dialog"] button[jsaction*=".dismiss"]"#];

/// "Reject all" on the consent page. The two forms differ in a hidden `set_eom`
/// input — `true` on the reject form — which does not depend on the language.
pub const CONSENT_REJECT: &[&str] = &[
	r#"form[action*="consent.google.com/save"]:has(input[name="set_eom"][value="true"]) button"#,
	r#"button[aria-label*="Reject all" i]"#,
	r#"button[aria-label*="Tout refuser" i]"#,
	r#"button[aria-label*="Alle ablehnen" i]"#,
	r#"button[aria-label*="Rechazar todo" i]"#,
	r#"button[aria-label*="Rifiuta tutto" i]"#,
];

/// The control that opens the review list: the tab, else the review count under the rating.
/// Tab labels are `"<name> - Avis"` / `"Reviews for <name>"`, so they are matched anywhere.
pub const REVIEWS_TAB: &[&str] = &[
	r#"button[role="tab"][aria-label*="review" i]"#,
	r#"button[role="tab"][aria-label*="Avis" i]"#,
	r#"button[role="tab"][aria-label*="Rezension" i]"#,
	r#"button[role="tab"][aria-label*="Reseña" i]"#,
	r#"button[role="tab"][aria-label*="Recension" i]"#,
	r#"button[jsaction*="reviewChart.moreReviews"]"#,
];

/// The place's name heading: present once the place panel has rendered.
pub const PLACE_TITLE: &[&str] = &["h1.DUwDvf", r#"[role="main"] h1"#];
/// The star average under the name ("4.6 stars"). A place without reviews has none.
pub const RATING_SUMMARY: &[&str] = &[r#".F7nice [role="img"][aria-label]"#];
/// The per-star rows of the review list's histogram, labelled "5 stars, 4,377 reviews" /
/// "5 étoiles, 396 164 avis". Their sum is how many reviews the list holds.
pub const HISTOGRAM_ROW: &str = r#"table tr[role="img"][aria-label]"#;

/// The sort menu button of the review list.
pub const SORT_BUTTON: &[&str] = &[
	r#"button[data-value="Sort"]"#,
	r#"button[data-value="Trier"]"#,
	r#"button[aria-label*="Sort" i]"#,
	r#"button[aria-label*="Trier" i]"#,
	r#"button[aria-label*="Sortieren" i]"#,
	r#"button[aria-label*="Ordenar" i]"#,
	r#"button[aria-label*="Ordina" i]"#,
];

/// "Newest" in the sort menu: the second entry, whatever it is called.
pub const SORT_NEWEST: &[&str] = &[r#"[role="menuitemradio"][data-index="1"]"#, r#"#action-menu [data-index="1"]"#];

/// A review card. Inner buttons carry the same attribute, so only the outermost element
/// with it counts (the parser and the in-page scripts both apply that rule).
pub const CARD: &str = "[data-review-id]";
/// The attribute holding the review's id.
pub const CARD_ID_ATTR: &str = "data-review-id";

/// "More" on a truncated review or owner reply.
pub const EXPAND: &[&str] = &[r#"button[jsaction*="review.expandReview"]"#, r#"button[jsaction*="expandOwnerResponse"]"#, "button.w8nwRe"];

/// The reviewer's display name.
pub const AUTHOR_NAME: &[&str] = &[".d4r55", r#"[class*="d4r55"]"#];
/// The author's profile link, as `data-href` or `href`.
pub const AUTHOR_LINK: &[&str] = &[r#"[data-href*="/maps/contrib/"]"#, r#"a[href*="/maps/contrib/"]"#];
/// Star rating: an `role=img` whose `aria-label` holds the number ("5 stars", "5 étoiles").
pub const RATING_STARS: &[&str] = &[r#"span[role="img"][aria-label]"#, r#"[role="img"][aria-label]"#];
/// Hotels and some other categories print "4/5" instead of stars.
pub const RATING_TEXT: &[&str] = &[".fzvQIb"];
/// The relative date ("3 weeks ago").
pub const DATE: &[&str] = &[".rsqaWe", ".xRkPPb"];
/// The owner's response block. Everything inside it is excluded from the review itself.
pub const REPLY_BLOCK: &[&str] = &[".CDe7pd"];
/// Body text, of the review and (inside `REPLY_BLOCK`) of the reply.
pub const BODY_TEXT: &[&str] = &[".MyEned .wiI7pd", ".wiI7pd"];
/// The reply's text, inside `REPLY_BLOCK`.
pub const REPLY_TEXT: &[&str] = &[".wiI7pd", ".CDe7pd div:last-child"];
/// One element per photo tile.
pub const PHOTO: &[&str] = &["button[data-photo-index]", "button.Tya61d"];
/// The last tile of a crowded grid stands for the rest: "+ 5", labelled "5 other photos".
/// Counted as that many photos instead of one. (Whether the tile's own image is among the
/// five is not stated anywhere on the page; this reads the label literally.)
pub const PHOTO_MORE: &[&str] = &[r#"button[jsaction*="showMorePhotos"]"#];
/// The number on that tile.
pub const PHOTO_MORE_COUNT: &[&str] = &[".Tap5If"];

/// In-page helpers. Selectors reach them as JSON arguments, so this file stays the only
/// place a selector is spelled.
pub mod js {
	/// `(selectors) => bool`: clicks the first element matching any selector.
	pub const CLICK_FIRST: &str = r#"(sels) => {
		for (const s of sels) {
			const el = document.querySelector(s);
			if (el) { el.click(); return true; }
		}
		return false;
	}"#;

	/// `(texts) => bool`: whether the page text contains any of them.
	pub const HAS_TEXT: &str = r#"(texts) => {
		const body = (document.body && document.body.innerText || "").replace(/\u00a0/g, " ");
		return texts.some(t => body.includes(t));
	}"#;

	/// `(selectors) => bool`: whether any selector matches.
	pub const ANY: &str = r#"(sels) => sels.some(s => document.querySelector(s))"#;

	/// `(sel) => [label]`: the `aria-label` of every element matching `sel`, in order.
	pub const LABELS: &str = r#"(sel) => Array.from(document.querySelectorAll(sel), el => el.getAttribute("aria-label") || "")"#;

	/// `(cardSel, idAttr) => id`: the id of the first card, "" without one (CDP returns no value for null).
	pub const FIRST_CARD_ID: &str = r#"(cardSel, idAttr) => {
		const el = document.querySelector(cardSel);
		return (el && el.getAttribute(idAttr)) || "";
	}"#;

	/// `(cardSel, expandSels) => n`: clicks every "More" inside a card; returns how many.
	pub const EXPAND_ALL: &str = r#"(cardSel, expandSels) => {
		let n = 0;
		for (const s of expandSels) {
			for (const b of document.querySelectorAll(s)) {
				if (!b.closest(cardSel)) continue;
				if (b.getAttribute("aria-expanded") === "true") continue;
				b.click(); n++;
			}
		}
		return n;
	}"#;

	/// `(cardSel, skip) => html`: the outer HTML of the outermost cards from the `skip`th on,
	/// in order — what the parser reads, and what `--dump-html` saves as a fixture.
	pub const CARDS_HTML: &str = r#"(cardSel, skip) => {
		const out = [];
		for (const el of document.querySelectorAll(cardSel)) {
			if (el.parentElement && el.parentElement.closest(cardSel)) continue;
			out.push(el);
		}
		return out.slice(skip).map(el => el.outerHTML).join("\n");
	}"#;

	/// `(cardSel) => bool`: scrolls the nearest scrollable ancestor of the cards to its end.
	pub const SCROLL_FEED: &str = r#"(cardSel) => {
		const cards = document.querySelectorAll(cardSel);
		if (!cards.length) return false;
		let el = cards[cards.length - 1].parentElement;
		while (el && !(el.scrollHeight > el.clientHeight && /(auto|scroll)/.test(getComputedStyle(el).overflowY))) {
			el = el.parentElement;
		}
		if (!el) return false;
		el.scrollTop = el.scrollHeight;
		return true;
	}"#;

	/// `(cardSel, idAttr, id) => bool`: tags the outermost card with this id for a screenshot.
	pub const MARK_CARD: &str = r#"(cardSel, idAttr, id) => {
		for (const el of document.querySelectorAll("[data-ra-capture]")) el.removeAttribute("data-ra-capture");
		for (const el of document.querySelectorAll(cardSel)) {
			if (el.getAttribute(idAttr) !== id) continue;
			if (el.parentElement && el.parentElement.closest(cardSel)) continue;
			el.setAttribute("data-ra-capture", "1");
			el.scrollIntoView({ block: "center", behavior: "instant" });
			return true;
		}
		return false;
	}"#;

	/// The card `MARK_CARD` tagged.
	pub const MARKED_CARD: &str = r#"[data-ra-capture="1"]"#;
}
