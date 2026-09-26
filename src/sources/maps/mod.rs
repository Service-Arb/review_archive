//! `maps`: any public place, read from its Google Maps page in a headless browser.

pub mod browser;
pub mod parse;
pub mod profile;
pub mod selectors;

use std::path::PathBuf;

use jiff::Timestamp;

use self::{
	browser::{SCREEN, Session, WalkEnd, WalkPolicy, Walked},
	parse::Card,
};
use crate::{
	config::{BrowserConfig, Defaults},
	domain::{Coverage, Known, Observed, ReviewSource, Scan, Target, relative_date},
};

/// A browser launched on first use and shared by every target of one pass.
pub struct Browser {
	cfg: BrowserConfig,
	profile_dir: PathBuf,
	dump_html: Option<PathBuf>,
	session: tokio::sync::Mutex<Option<Session>>,
}

impl Browser {
	pub fn new(cfg: BrowserConfig, profile_dir: PathBuf, dump_html: Option<PathBuf>) -> Self {
		Self {
			cfg,
			profile_dir,
			dump_html,
			session: tokio::sync::Mutex::new(None),
		}
	}

	/// Opens the place's review list and walks it.
	pub async fn walk(&self, place_id: &str, lang: &str, policy: &mut dyn WalkPolicy, max: usize) -> eyre::Result<Walked> {
		let mut guard = self.session.lock().await;
		if guard.is_none() {
			let mut s = Session::launch(&self.cfg, &self.profile_dir).await?;
			s.dump_html = self.dump_html.clone();
			*guard = Some(s);
		}
		let session = guard.as_ref().expect("launched just above");
		match session.open_reviews(place_id, lang).await? {
			Some(opened) => session.walk(policy, max, opened).await,
			None => Ok(Walked {
				cards: Vec::new(),
				end: WalkEnd::ReachedEnd,
				page_url: selectors::place_url(place_id, lang),
				warnings: Vec::new(),
				sorted: true,
				total: Some(0),
			}),
		}
	}

	pub async fn close(&self) {
		if let Some(s) = self.session.lock().await.take() {
			s.close().await;
		}
	}
}

pub struct MapsSource<'a> {
	pub browser: &'a Browser,
	pub defaults: &'a Defaults,
}

struct NewestFirst<'a> {
	known: &'a Known,
	/// Archived cards in a row, up to the last one read.
	known_run: usize,
}

impl WalkPolicy for NewestFirst<'_> {
	fn wants_capture(&self, card: &Card) -> bool {
		self.known.wants_capture(&card.id)
	}

	fn observe(&mut self, card: &Card) {
		self.known_run = if self.known.contains(&card.id) { self.known_run + 1 } else { 0 };
	}

	/// A full screen of archived cards in a row: everything older is archived too.
	fn satisfied(&self) -> bool {
		self.known_run >= SCREEN
	}
}

impl ReviewSource for MapsSource<'_> {
	async fn scan(&self, target: &Target, known: &Known) -> eyre::Result<Scan> {
		let max = if known.is_empty() {
			self.defaults.max_reviews_initial
		} else {
			self.defaults.max_reviews_per_scan
		};
		let mut policy = NewestFirst { known, known_run: 0 };
		let walked = self.browser.walk(&target.place_id, &target.lang, &mut policy, max).await?;
		Ok(to_scan(walked, max, Timestamp::now()))
	}
}

pub fn observed(card: Card, now: Timestamp) -> Observed {
	let published_est = card.date_raw.as_deref().and_then(|d| relative_date::estimate(d, now));
	Observed {
		source_review_id: card.id,
		author: card.author,
		author_url: card.author_url,
		rating: card.rating,
		text: card.text,
		reply: card.reply,
		photo_count: card.photo_count,
		published_raw: card.date_raw,
		published_est,
		capture: None,
	}
}

fn to_scan(walked: Walked, max: usize, now: Timestamp) -> Scan {
	let Walked {
		cards,
		end,
		mut warnings,
		sorted,
		total,
		..
	} = walked;
	if end == WalkEnd::Cap {
		warnings.push(format!("stopped after {max} reviews without reaching archived ones or the end of the list"));
	}
	let reviews: Vec<Observed> = cards.into_iter().map(|(card, capture)| Observed { capture, ..observed(card, now) }).collect();
	let coverage = coverage(&reviews, end, sorted, total, &mut warnings);
	Scan { reviews, coverage, warnings }
}

/// How far a walk can be trusted to have looked.
///
/// The whole list only when the page's own count agrees: an idle feed may just be a lazy
/// load that stalled. A newest-first prefix only when the list was seen to re-sort and its
/// dates run newest first; otherwise, nothing — a relevance-sorted list says nothing about
/// what is missing from it.
fn coverage(reviews: &[Observed], end: WalkEnd, sorted: bool, total: Option<u64>, warnings: &mut Vec<String>) -> Coverage {
	let seen = u64::try_from(reviews.len()).unwrap_or(u64::MAX);
	if end == WalkEnd::ReachedEnd && total.is_some_and(|t| seen >= t) {
		return Coverage::Complete;
	}
	if end == WalkEnd::ReachedEnd {
		tracing::info!(seen, ?total, "the feed stopped growing short of the page's review count");
	}
	if !sorted {
		return Coverage::DownTo(None);
	}
	let dates: Vec<Timestamp> = reviews.iter().filter_map(|r| r.published_est).collect();
	if dates.windows(2).any(|w| w[1] > w[0]) {
		warnings.push("the review list is not in newest-first order; no review is judged gone this run".to_owned());
		return Coverage::DownTo(None);
	}
	Coverage::DownTo(dates.last().copied())
}

#[cfg(test)]
mod tests {
	use super::*;

	type WalkedCard = (Card, Option<crate::domain::Capture>);

	fn card(id: &str, date: &str) -> WalkedCard {
		(
			Card {
				id: id.into(),
				author: "A".into(),
				rating: Some(5),
				date_raw: Some(date.into()),
				..Default::default()
			},
			None,
		)
	}

	fn walked(cards: Vec<WalkedCard>, end: WalkEnd, sorted: bool, total: Option<u64>) -> Walked {
		Walked {
			cards,
			end,
			page_url: String::new(),
			warnings: vec![],
			sorted,
			total,
		}
	}

	fn now() -> Timestamp {
		"2026-09-26T12:00:00Z".parse().unwrap()
	}

	fn newest_first() -> Vec<WalkedCard> {
		vec![card("a", "a day ago"), card("b", "a week ago"), card("c", "2 months ago")]
	}

	#[test]
	fn the_end_of_the_feed_is_complete_only_when_the_count_agrees() {
		assert_eq!(to_scan(walked(newest_first(), WalkEnd::ReachedEnd, true, Some(3)), 200, now()).coverage, Coverage::Complete);
		// a stalled lazy load: the feed went idle with most of the list unseen
		let stalled = to_scan(walked(newest_first(), WalkEnd::ReachedEnd, true, Some(4000)), 200, now());
		assert_eq!(stalled.coverage, Coverage::DownTo(Some("2026-07-26T12:00:00Z".parse().unwrap())));
		// no count on the page: same
		let uncounted = to_scan(walked(newest_first(), WalkEnd::ReachedEnd, true, None), 200, now());
		assert!(matches!(uncounted.coverage, Coverage::DownTo(Some(_))));
	}

	#[test]
	fn an_unconfirmed_sort_concludes_nothing() {
		let s = to_scan(walked(newest_first(), WalkEnd::Satisfied, false, Some(4000)), 200, now());
		assert_eq!(s.coverage, Coverage::DownTo(None));
		// a short list read to its end is complete whatever its order
		let all = to_scan(walked(newest_first(), WalkEnd::ReachedEnd, false, Some(3)), 200, now());
		assert_eq!(all.coverage, Coverage::Complete);
	}

	#[test]
	fn a_list_out_of_date_order_concludes_nothing() {
		let shuffled = vec![card("a", "a week ago"), card("b", "a year ago"), card("c", "a day ago")];
		let s = to_scan(walked(shuffled, WalkEnd::Satisfied, true, None), 200, now());
		assert_eq!(s.coverage, Coverage::DownTo(None));
		assert_eq!(s.warnings.len(), 1);
	}

	#[test]
	fn a_place_without_reviews_is_an_empty_complete_scan() {
		let s = to_scan(walked(vec![], WalkEnd::ReachedEnd, true, Some(0)), 200, now());
		assert_eq!((s.reviews.len(), s.coverage), (0, Coverage::Complete));
		assert!(s.warnings.is_empty());
	}
}
