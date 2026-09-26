//! Google Maps review lists, without the browser: the markup, the parser, and what a
//! walk down the list is allowed to conclude.
//!
//! Whoever drives the browser (the engine's `Browser`, or your own) reads cards with
//! [`parse::cards`], asks a [`WalkPolicy`] which to screenshot and when to stop, and
//! turns the result into a [`Scan`] with [`scan_of`].

pub mod parse;
pub mod selectors;

use std::collections::HashSet;

use jiff::Timestamp;
pub use parse::Card;

use crate::{Capture, Coverage, Known, Observed, Scan, relative_date};

/// Cards per screen of the feed: a run this long of already-archived cards ends a walk.
pub const SCREEN: usize = 10;

/// What a walk needs to know about each card, and when it has seen enough.
pub trait WalkPolicy: Send {
	/// Whether to screenshot this card.
	fn wants_capture(&self, card: &Card) -> bool;
	/// Each card the walk reads, once, in list order.
	fn observe(&mut self, card: &Card);
	/// Checked after each step; `true` ends the walk early.
	fn satisfied(&self) -> bool;
}

/// Why a walk stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkEnd {
	/// Scrolling stopped producing cards, or there was nothing to scroll. Most likely the
	/// whole list — but a lazy load that stalls looks the same, so it proves nothing alone.
	ReachedEnd,
	/// The policy had what it needed.
	Satisfied,
	/// `max` cards were read.
	Cap,
}

/// A card and its screenshot, if one was taken.
pub type WalkedCard = (Card, Option<Capture>);

/// What a walk down a review list read.
#[derive(Debug)]
pub struct Walked {
	/// In list order.
	pub cards: Vec<WalkedCard>,
	/// Why it stopped.
	pub end: WalkEnd,
	/// The page it walked.
	pub page_url: String,
	/// Things that went wrong without ending the walk (a failed screenshot).
	pub warnings: Vec<String>,
	/// Whether the list was seen to re-sort to newest first. When not, its order is unknown.
	pub sorted: bool,
	/// How many reviews the page says the list holds, when it says.
	pub total: Option<u64>,
}

impl Walked {
	/// The walk of a place that has no reviews: nothing to read, and that is all of it.
	pub fn empty(page_url: String) -> Self {
		Self {
			cards: Vec::new(),
			end: WalkEnd::ReachedEnd,
			page_url,
			warnings: Vec::new(),
			sorted: true,
			total: Some(0),
		}
	}
}

/// The scan of a target already archived: capture what is new or pending, stop after a
/// full screen of archived cards in a row (everything older is archived too).
#[derive(Debug)]
pub struct NewestFirst<'a> {
	known: &'a Known,
	known_run: usize,
}

impl<'a> NewestFirst<'a> {
	/// A policy against what `known` holds.
	pub fn new(known: &'a Known) -> Self {
		Self { known, known_run: 0 }
	}
}

impl WalkPolicy for NewestFirst<'_> {
	fn wants_capture(&self, card: &Card) -> bool {
		self.known.wants_capture(&card.id)
	}

	fn observe(&mut self, card: &Card) {
		self.known_run = if self.known.contains(&card.id) { self.known_run + 1 } else { 0 };
	}

	fn satisfied(&self) -> bool {
		self.known_run >= SCREEN
	}
}

/// A one-off capture: screenshot every card, or only the named ones — and then stop as
/// soon as they are all found.
#[derive(Debug, Default)]
pub struct CaptureAll {
	wanted: Option<HashSet<String>>,
}

impl CaptureAll {
	/// Every card, down to the walk's limit.
	pub fn every() -> Self {
		Self { wanted: None }
	}

	/// Only these review ids.
	pub fn only(ids: impl IntoIterator<Item = String>) -> Self {
		Self {
			wanted: Some(ids.into_iter().collect()),
		}
	}
}

impl WalkPolicy for CaptureAll {
	fn wants_capture(&self, card: &Card) -> bool {
		self.wanted.as_ref().is_none_or(|w| w.contains(&card.id))
	}

	fn observe(&mut self, card: &Card) {
		if let Some(w) = &mut self.wanted {
			w.remove(&card.id);
		}
	}

	fn satisfied(&self) -> bool {
		self.wanted.as_ref().is_some_and(HashSet::is_empty)
	}
}

/// A card as an observation, its relative date estimated against `now`.
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

/// The scan a walk amounts to. `max` is the limit it was given, for the warning when it
/// was hit.
pub fn scan_of(walked: Walked, max: usize, now: Timestamp) -> Scan {
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
		assert_eq!(scan_of(walked(newest_first(), WalkEnd::ReachedEnd, true, Some(3)), 200, now()).coverage, Coverage::Complete);
		// a stalled lazy load: the feed went idle with most of the list unseen
		let stalled = scan_of(walked(newest_first(), WalkEnd::ReachedEnd, true, Some(4000)), 200, now());
		assert_eq!(stalled.coverage, Coverage::DownTo(Some("2026-07-26T12:00:00Z".parse().unwrap())));
		// no count on the page: same
		let uncounted = scan_of(walked(newest_first(), WalkEnd::ReachedEnd, true, None), 200, now());
		assert!(matches!(uncounted.coverage, Coverage::DownTo(Some(_))));
	}

	#[test]
	fn an_unconfirmed_sort_concludes_nothing() {
		let s = scan_of(walked(newest_first(), WalkEnd::Satisfied, false, Some(4000)), 200, now());
		assert_eq!(s.coverage, Coverage::DownTo(None));
		// a short list read to its end is complete whatever its order
		let all = scan_of(walked(newest_first(), WalkEnd::ReachedEnd, false, Some(3)), 200, now());
		assert_eq!(all.coverage, Coverage::Complete);
	}

	#[test]
	fn a_list_out_of_date_order_concludes_nothing() {
		let shuffled = vec![card("a", "a week ago"), card("b", "a year ago"), card("c", "a day ago")];
		let s = scan_of(walked(shuffled, WalkEnd::Satisfied, true, None), 200, now());
		assert_eq!(s.coverage, Coverage::DownTo(None));
		assert_eq!(s.warnings.len(), 1);
	}

	#[test]
	fn a_place_without_reviews_is_an_empty_complete_scan() {
		let s = scan_of(Walked::empty(String::new()), 200, now());
		assert_eq!((s.reviews.len(), s.coverage), (0, Coverage::Complete));
		assert!(s.warnings.is_empty());
	}

	#[test]
	fn newest_first_stops_after_a_screen_of_archived_cards() {
		let known = Known {
			reviews: (0..SCREEN)
				.map(|i| {
					(
						format!("k{i}"),
						crate::KnownReview {
							id: crate::ReviewId(i as i64),
							content_hash: String::new(),
							capture_pending: false,
							gone: false,
							published_est: None,
							published_raw: None,
							author: String::new(),
							rating: None,
							text: None,
						},
					)
				})
				.collect(),
		};
		let mut p = NewestFirst::new(&known);
		p.observe(&card("new", "a day ago").0);
		for i in 0..SCREEN - 1 {
			p.observe(&card(&format!("k{i}"), "a week ago").0);
		}
		assert!(!p.satisfied());
		p.observe(&card(&format!("k{}", SCREEN - 1), "a week ago").0);
		assert!(p.satisfied());
		assert!(p.wants_capture(&card("new", "").0) && !p.wants_capture(&card("k0", "").0));
	}

	#[test]
	fn capture_all_named_stops_when_all_are_found() {
		let mut p = CaptureAll::only(["b".to_owned()]);
		assert!(!p.wants_capture(&card("a", "").0) && p.wants_capture(&card("b", "").0));
		p.observe(&card("a", "").0);
		assert!(!p.satisfied());
		p.observe(&card("b", "").0);
		assert!(p.satisfied());
		assert!(!CaptureAll::every().satisfied());
	}
}
