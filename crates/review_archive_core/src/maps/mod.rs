//! Google Maps review lists, without the browser: the markup, the parser, and what a
//! walk down the list is allowed to conclude.
//!
//! Whoever drives the browser (the engine's `Browser`, or your own) reads cards with
//! [`parse::cards`], asks a [`WalkPolicy`] which to screenshot and when to stop, and
//! turns the result into a [`Scan`] with [`NewestFirst::conclude`], [`Requested::conclude`]
//! or [`scan_of`].

pub mod parse;
pub mod selectors;

use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
pub use parse::Card;

use crate::{Capture, Coverage, Known, Observed, OwnerPost, Scan, relative_date};

/// Cards per screen of the feed: a run this long of already-archived cards ends a walk.
pub const SCREEN: usize = 10;

/// What each action of a walk costs in tokens: the data requests it sends Google, in units of a
/// feed scroll's ~9. Measured on 18-, 400- and 1000+-review places; "More" and screenshots send none.
pub mod cost {
	/// Opening the place's page (and answering the consent page).
	pub const OPEN: i64 = 7;
	/// The Reviews tab, and its histogram.
	pub const REVIEWS: i64 = 1;
	/// The sort menu and "Newest".
	pub const SORT: i64 = 7;
	/// Each click on the sort button past the first.
	pub const SORT_RETRY: i64 = 2;
	/// One scroll of the feed, and the screen of cards it loads.
	pub const STEP: i64 = 1;
	/// A walk is not started on less: with it, it reads at least the list's first screen.
	pub const FIRST_SCREEN: i64 = OPEN + REVIEWS + SORT;
}

/// What a walk needs to know about each card, and when it has seen enough.
pub trait WalkPolicy: Send {
	/// Whether to read the list at all, given how many reviews the page says it holds.
	fn wants_list(&self, total: Option<u64>) -> bool;
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
	/// The page failed under the walk after it had read cards; what it read stands, and a
	/// warning says why it stopped.
	Interrupted,
	/// The policy did not want the list read ([`WalkPolicy::wants_list`]).
	Unread,
	/// The next scroll would have spent more tokens than the walk was given.
	Budget,
}

impl WalkEnd {
	/// Stopped before the list ran out or the policy was done.
	pub fn cut_short(self) -> bool {
		matches!(self, Self::Cap | Self::Interrupted | Self::Budget)
	}
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
	/// The owner's latest post, from the place's overview.
	pub post: Option<parse::Post>,
}

impl Walked {
	/// The walk of a place that has no reviews: nothing to read, and that is all of it.
	pub fn empty(page_url: String, post: Option<parse::Post>) -> Self {
		Self {
			cards: Vec::new(),
			end: WalkEnd::ReachedEnd,
			page_url,
			warnings: Vec::new(),
			sorted: true,
			total: Some(0),
			post,
		}
	}

	/// The walk of a list the policy did not want read: nothing read, nothing judged.
	pub fn unchanged(page_url: String, total: Option<u64>, post: Option<parse::Post>) -> Self {
		Self {
			cards: Vec::new(),
			end: WalkEnd::Unread,
			page_url,
			warnings: Vec::new(),
			sorted: false,
			total,
			post,
		}
	}

	fn last_id(&self) -> Option<String> {
		self.cards.last().map(|(c, _)| c.id.clone())
	}
}

/// The scan of a target: capture what is new or pending, and stop after a full screen of
/// archived cards in a row (everything older is archived too) — but only once past where
/// the last walk was cut short, with every pending review found or passed, and never on a
/// target's first scan, which reads the whole list.
#[derive(Debug)]
pub struct NewestFirst<'a> {
	known: &'a Known,
	now: Timestamp,
	/// Archived, not pending, in a row.
	known_run: usize,
	/// Read a full screen of archived cards past the cut: the rest of the list is archived.
	caught_up: bool,
	/// Past [`Known::cut_after`] (from the start, when there is none).
	past_cut: bool,
	/// Pending reviews not yet read, with the earliest each can have been posted.
	pending: HashMap<&'a str, Timestamp>,
	/// The latest the last dated card read can have been posted.
	last_card_at: Option<Timestamp>,
}

impl<'a> NewestFirst<'a> {
	/// A policy against what `known` holds; `now` dates the cards' relative dates.
	pub fn new(known: &'a Known, now: Timestamp) -> Self {
		let pending = known
			.reviews
			.iter()
			.filter(|(_, k)| k.capture_pending && !k.gone)
			.filter_map(|(id, k)| Some((id.as_str(), k.earliest()?)))
			.collect();
		Self {
			known,
			now,
			known_run: 0,
			caught_up: false,
			past_cut: known.cut_after.is_none(),
			pending,
			last_card_at: None,
		}
	}

	/// Pending reviews the walk has neither read nor gone past.
	fn unreached(&self) -> usize {
		self.pending.values().filter(|&&earliest| self.last_card_at.is_none_or(|last| earliest <= last)).count()
	}

	/// The scan the walk amounts to. `max` is the limit it was given: a scheduled scan that
	/// hits it before reaching archived cards leaves a gap, which is a warning — and
	/// [`Scan::cut_after`], so that the next scan fills it. So does one cut short with
	/// reviews still without a screenshot below it — once: a gap-filling scan that still
	/// cannot reach them leaves them pending. A walk that never got past the old cut keeps
	/// it, even at an idle feed. A target's first scan stopping at its limit leaves no gap:
	/// that limit is how deep the archive goes.
	pub fn conclude(&self, walked: Walked, max: usize, now: Timestamp) -> Scan {
		let end = walked.end;
		let last = walked.last_id();
		let unreached = self.unreached();
		let mut scan = scan_of(walked, now);
		let filling_gap = self.known.cut_after.is_some();
		let cut_after = if scan.coverage == Coverage::Complete {
			None
		} else if !self.past_cut {
			// not even past the old cut (a limit, a failure, or a feed that stalled): that gap
			// is still the one to fill
			self.known.cut_after.clone()
		} else if !end.cut_short() || (end == WalkEnd::Cap && self.known.initial) {
			// read to the end, or done; or a first scan at its limit, which is how deep the
			// archive goes
			None
		} else if !self.caught_up || (end == WalkEnd::Budget && self.known.initial) {
			// a first scan reads the whole list, whatever ad-hoc captures archived of it
			last
		} else if unreached > 0 && !filling_gap {
			// caught up, but reviews still without a screenshot lie deeper than this scan may
			// read: the next one reads deeper, once
			last
		} else {
			if unreached > 0 {
				scan.warnings
					.push(format!("{unreached} reviews still without a screenshot lie deeper than a scan reads; they stay pending"));
			}
			None
		};
		if end == WalkEnd::Cap && (self.known.initial || !self.caught_up) {
			scan.warnings.push(format!("stopped after {max} reviews without reaching archived ones or the end of the list"));
		}
		if end == WalkEnd::Budget && cut_after.is_some() {
			scan.warnings
				.push("out of tokens before the end of the list; the next scan goes on from its last card".to_owned());
		}
		if end.cut_short() && unreached > 0 && cut_after.is_some() {
			scan.warnings.push(format!("{unreached} reviews still without a screenshot were not reached"));
		}
		scan.cut_after = cut_after;
		scan
	}
}

impl WalkPolicy for NewestFirst<'_> {
	/// Not when the count is the last scan's and nothing is owed below the top of the list.
	fn wants_list(&self, total: Option<u64>) -> bool {
		let owed = self.known.initial || self.known.cut_after.is_some() || self.known.reviews.values().any(|k| k.capture_pending && !k.gone);
		owed || total.is_none() || total != self.known.listed
	}

	fn wants_capture(&self, card: &Card) -> bool {
		self.known.wants_capture(&card.id)
	}

	fn observe(&mut self, card: &Card) {
		let at = card.date_raw.as_deref().and_then(|d| relative_date::estimate(d, self.now));
		self.last_card_at = at.or(self.last_card_at);
		self.pending.remove(card.id.as_str());
		if !self.past_cut {
			let cut = self.known.cut_after.as_deref();
			// the cut card itself, or one that is surely older than it (it may be gone)
			let older = || at.zip(cut.and_then(|c| self.known.reviews.get(c)?.earliest())).is_some_and(|(at, cut)| at < cut);
			self.past_cut = cut == Some(card.id.as_str()) || older();
			return;
		}
		let archived = self.known.reviews.get(&card.id).is_some_and(|k| !k.capture_pending);
		self.known_run = if archived { self.known_run + 1 } else { 0 };
		self.caught_up |= self.known_run >= SCREEN;
	}

	fn satisfied(&self) -> bool {
		!self.known.initial && self.caught_up && self.unreached() == 0
	}
}

/// An ad-hoc capture: screenshot what `known` still lacks (every card, against an empty
/// one) of the named reviews when named, down to the walk's limit or until the named ones
/// are all found.
#[derive(Debug)]
pub struct Requested<'a> {
	known: &'a Known,
	wanted: Option<HashSet<String>>,
}

impl<'a> Requested<'a> {
	/// Against what `known` holds; `review_ids` narrows it to those.
	pub fn new(known: &'a Known, review_ids: Option<impl IntoIterator<Item = String>>) -> Self {
		Self {
			known,
			wanted: review_ids.map(|ids| ids.into_iter().collect()),
		}
	}

	/// The scan the walk amounts to. Reaching the limit is what was asked for, not a
	/// warning; it leaves a gap ([`Scan::cut_after`]) only on a target with an archive that
	/// the walk never reached.
	pub fn conclude(&self, walked: Walked, now: Timestamp) -> Scan {
		let reached_archive = walked.cards.iter().any(|(c, _)| self.known.contains(&c.id));
		let gap = matches!(walked.end, WalkEnd::Interrupted | WalkEnd::Budget) || (walked.end == WalkEnd::Cap && !self.known.initial && !reached_archive);
		let cut_after = gap.then(|| walked.last_id()).flatten();
		Scan { cut_after, ..scan_of(walked, now) }
	}
}

impl WalkPolicy for Requested<'_> {
	fn wants_list(&self, _: Option<u64>) -> bool {
		true
	}

	fn wants_capture(&self, card: &Card) -> bool {
		self.known.wants_capture(&card.id) && self.wanted.as_ref().is_none_or(|w| w.contains(&card.id))
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

/// The scan a walk amounts to: its cards as observations, and how far they can be trusted
/// to reach. No warning for a limit and no gap; the policies' `conclude` add those.
pub fn scan_of(walked: Walked, now: Timestamp) -> Scan {
	let Walked {
		cards,
		end,
		mut warnings,
		sorted,
		total,
		post,
		..
	} = walked;
	let reviews: Vec<Observed> = cards.into_iter().map(|(card, capture)| Observed { capture, ..observed(card, now) }).collect();
	let coverage = coverage(&reviews, end, sorted, total, &mut warnings);
	Scan {
		reviews,
		coverage,
		warnings,
		cut_after: None,
		listed: total,
		post: post.map(|p| OwnerPost {
			published_est: p.date_raw.as_deref().and_then(|d| relative_date::estimate(d, now)),
			published_raw: p.date_raw,
			text: p.text,
		}),
	}
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
	use crate::{KnownReview, ReviewId};

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
			post: None,
		}
	}

	fn now() -> Timestamp {
		"2026-09-26T12:00:00Z".parse().unwrap()
	}

	fn newest_first() -> Vec<WalkedCard> {
		vec![card("a", "a day ago"), card("b", "a week ago"), card("c", "2 months ago")]
	}

	/// `ids` archived, as first seen "a week ago" on 2026-09-01 unless pending.
	fn known(ids: impl IntoIterator<Item = String>, pending: &[&str]) -> Known {
		Known {
			reviews: ids
				.into_iter()
				.enumerate()
				.map(|(i, id)| {
					let review = KnownReview {
						id: ReviewId(i as i64),
						content_hash: String::new(),
						capture_pending: pending.contains(&id.as_str()),
						gone: false,
						published_est: Some("2026-08-25T00:00:00Z".parse().unwrap()),
						published_raw: Some("a week ago".into()),
						author: String::new(),
						rating: None,
						text: None,
					};
					(id, review)
				})
				.collect(),
			..Default::default()
		}
	}

	#[test]
	fn the_end_of_the_feed_is_complete_only_when_the_count_agrees() {
		assert_eq!(scan_of(walked(newest_first(), WalkEnd::ReachedEnd, true, Some(3)), now()).coverage, Coverage::Complete);
		// a stalled lazy load: the feed went idle with most of the list unseen
		let stalled = scan_of(walked(newest_first(), WalkEnd::ReachedEnd, true, Some(4000)), now());
		assert_eq!(stalled.coverage, Coverage::DownTo(Some("2026-07-26T12:00:00Z".parse().unwrap())));
		// no count on the page: same
		let uncounted = scan_of(walked(newest_first(), WalkEnd::ReachedEnd, true, None), now());
		assert!(matches!(uncounted.coverage, Coverage::DownTo(Some(_))));
	}

	#[test]
	fn an_unconfirmed_sort_concludes_nothing() {
		let s = scan_of(walked(newest_first(), WalkEnd::Satisfied, false, Some(4000)), now());
		assert_eq!(s.coverage, Coverage::DownTo(None));
		// a short list read to its end is complete whatever its order
		let all = scan_of(walked(newest_first(), WalkEnd::ReachedEnd, false, Some(3)), now());
		assert_eq!(all.coverage, Coverage::Complete);
	}

	#[test]
	fn a_list_out_of_date_order_concludes_nothing() {
		let shuffled = vec![card("a", "a week ago"), card("b", "a year ago"), card("c", "a day ago")];
		let s = scan_of(walked(shuffled, WalkEnd::Satisfied, true, None), now());
		assert_eq!(s.coverage, Coverage::DownTo(None));
		assert_eq!(s.warnings.len(), 1);
	}

	#[test]
	fn a_requested_limit_is_not_a_warning() {
		let w = || walked(newest_first(), WalkEnd::Cap, true, Some(4000));
		let archived = known(["z".to_owned()], &[]);
		assert_eq!(NewestFirst::new(&archived, now()).conclude(w(), 3, now()).warnings.len(), 1);
		let s = Requested::new(&Known::default(), None::<Vec<String>>).conclude(w(), now());
		assert!(s.warnings.is_empty());
		assert!(matches!(s.coverage, Coverage::DownTo(Some(_))));
	}

	#[test]
	fn a_place_without_reviews_is_an_empty_complete_scan() {
		let s = NewestFirst::new(&Known::default(), now()).conclude(Walked::empty(String::new(), None), 200, now());
		assert_eq!((s.reviews.len(), s.coverage, s.cut_after), (0, Coverage::Complete, None));
		assert!(s.warnings.is_empty());
	}

	#[test]
	fn newest_first_stops_after_a_screen_of_archived_cards() {
		let known = known((0..SCREEN).map(|i| format!("k{i}")), &[]);
		let mut p = NewestFirst::new(&known, now());
		p.observe(&card("new", "a day ago").0);
		for i in 0..SCREEN - 1 {
			p.observe(&card(&format!("k{i}"), "a week ago").0);
		}
		assert!(!p.satisfied());
		p.observe(&card(&format!("k{}", SCREEN - 1), "a week ago").0);
		assert!(p.satisfied());
		assert!(p.wants_capture(&card("new", "").0) && !p.wants_capture(&card("k0", "").0));

		// a first scan reads on regardless
		let first = Known { initial: true, ..known.clone() };
		let mut p = NewestFirst::new(&first, now());
		(0..SCREEN).for_each(|i| p.observe(&card(&format!("k{i}"), "a week ago").0));
		assert!(!p.satisfied());
	}

	/// A scan capped among new cards leaves the rest unread; the next one reads past where
	/// it stopped before archived cards can end it, and says where to resume if it is cut
	/// short again.
	#[test]
	fn a_walk_cut_short_is_resumed_past_where_it_stopped() {
		let archived = known(["old".to_owned()], &[]);
		let capped = walked(vec![card("n1", "a day ago"), card("n2", "a day ago")], WalkEnd::Cap, true, None);
		let first = NewestFirst::new(&archived, now()).conclude(capped, 2, now());
		assert_eq!(first.cut_after.as_deref(), Some("n2"));
		assert_eq!(first.warnings.len(), 1);

		let mut resumed = known((0..SCREEN).map(|i| format!("n{i}")), &[]);
		resumed.cut_after = Some(format!("n{}", SCREEN - 1));
		let mut p = NewestFirst::new(&resumed, now());
		(0..SCREEN).for_each(|i| p.observe(&card(&format!("n{i}"), "a week ago").0));
		assert!(!p.satisfied(), "still above the cut");
		// cut short again before getting past the old cut: that one stays
		let again = walked(vec![card("n0", "a week ago")], WalkEnd::Interrupted, true, None);
		let mut early = NewestFirst::new(&resumed, now());
		early.observe(&card("n0", "a week ago").0);
		assert_eq!(early.conclude(again, 2000, now()).cut_after, resumed.cut_after);

		// past it, a screen of archived cards ends the walk: the gap is closed
		(0..SCREEN).for_each(|i| p.observe(&card(&format!("n{i}"), "a week ago").0));
		assert!(p.satisfied());
		let closed = p.conclude(walked(vec![], WalkEnd::Satisfied, true, None), 2000, now());
		assert_eq!(closed.cut_after, None);
	}

	/// A pending card deeper than the first screen keeps the walk going until it is read or
	/// passed; not reaching it is a warning.
	#[test]
	fn pending_reviews_are_walked_to() {
		let mut ids: Vec<String> = (0..SCREEN).map(|i| format!("k{i}")).collect();
		ids.push("pending".into());
		let archived = known(ids, &["pending"]);
		let mut p = NewestFirst::new(&archived, now());
		(0..SCREEN).for_each(|i| p.observe(&card(&format!("k{i}"), "a day ago").0));
		assert!(!p.satisfied(), "the pending one (posted 2026-08-18 at the earliest) is further down");
		let capped = p.conclude(walked(vec![card("k0", "a day ago")], WalkEnd::Cap, true, None), 10, now());
		assert_eq!(capped.warnings, ["1 reviews still without a screenshot were not reached"]);
		assert_eq!(capped.cut_after.as_deref(), Some("k0"), "the next scan reads deeper, with the larger limit");
		p.observe(&card("older", "2 months ago").0);
		assert!(p.satisfied(), "walked past it: it is gone from the list, or its date moved");
	}

	/// A pending review deeper than even the gap-filling scan reads is not chased forever:
	/// that scan gives up on it rather than set another cut.
	#[test]
	fn a_pending_review_out_of_reach_is_given_up_on() {
		let mut ids: Vec<String> = (0..SCREEN).map(|i| format!("k{i}")).collect();
		ids.extend(["pending".into(), "cut".into()]);
		let mut deep = known(ids, &["pending"]);
		deep.cut_after = Some("cut".into());
		let mut p = NewestFirst::new(&deep, now());
		p.observe(&card("cut", "a day ago").0);
		(0..SCREEN).for_each(|i| p.observe(&card(&format!("k{i}"), "a day ago").0));
		let s = p.conclude(walked(vec![card("k9", "a day ago")], WalkEnd::Cap, true, None), 2000, now());
		assert_eq!(s.cut_after, None);
		assert_eq!(s.warnings, ["1 reviews still without a screenshot lie deeper than a scan reads; they stay pending"]);
	}

	/// The end of a feed that may have stalled does not close a gap the walk never got to;
	/// a walk that got past the cut to the end of the feed does.
	#[test]
	fn an_idle_feed_keeps_a_gap_it_did_not_reach() {
		let mut gap = known((0..3).map(|i| format!("n{i}")), &[]);
		gap.cut_after = Some("n2".into());
		let mut p = NewestFirst::new(&gap, now());
		p.observe(&card("n0", "a week ago").0);
		let stalled = p.conclude(walked(vec![card("n0", "a week ago")], WalkEnd::ReachedEnd, true, Some(4000)), 2000, now());
		assert_eq!(stalled.cut_after.as_deref(), Some("n2"));

		p.observe(&card("n2", "a week ago").0);
		let ended = p.conclude(walked(vec![card("n2", "a week ago")], WalkEnd::ReachedEnd, true, None), 2000, now());
		assert_eq!(ended.cut_after, None);
	}

	/// Unlike its limit, a first scan out of tokens is not how deep the archive goes: the
	/// next scan goes on from its last card.
	#[test]
	fn a_first_scan_out_of_tokens_leaves_a_gap() {
		let first = Known { initial: true, ..Known::default() };
		let mut p = NewestFirst::new(&first, now());
		newest_first().iter().for_each(|(c, _)| p.observe(c));
		let s = p.conclude(walked(newest_first(), WalkEnd::Budget, true, Some(4000)), 2000, now());
		assert_eq!(s.cut_after.as_deref(), Some("c"));
		assert_eq!(s.warnings.len(), 1);

		// over cards ad-hoc captures archived already, too
		let captured = Known {
			initial: true,
			..known((0..SCREEN).map(|i| format!("k{i}")), &[])
		};
		let mut p = NewestFirst::new(&captured, now());
		(0..SCREEN).for_each(|i| p.observe(&card(&format!("k{i}"), "a week ago").0));
		let s = p.conclude(walked(vec![card("k9", "a week ago")], WalkEnd::Budget, true, Some(4000)), 2000, now());
		assert_eq!(s.cut_after.as_deref(), Some("k9"));
	}

	/// A page that fails after the walk caught up with the archive leaves nothing unread.
	#[test]
	fn an_interruption_after_catching_up_leaves_no_gap() {
		let archived = known((0..SCREEN).map(|i| format!("k{i}")), &[]);
		let mut p = NewestFirst::new(&archived, now());
		(0..SCREEN).for_each(|i| p.observe(&card(&format!("k{i}"), "a week ago").0));
		let s = p.conclude(walked(vec![card("k9", "a week ago")], WalkEnd::Interrupted, true, None), 200, now());
		assert_eq!(s.cut_after, None);
	}

	/// The same count as the last scan, with nothing owed below it, is not read: nothing is
	/// judged from it either. A pending capture, a gap, or a first scan reads regardless.
	#[test]
	fn an_unchanged_count_is_not_read() {
		let settled = Known {
			listed: Some(1),
			..known(["a".to_owned()], &[])
		};
		let p = NewestFirst::new(&settled, now());
		assert!(!p.wants_list(Some(1)));
		assert!(p.wants_list(Some(2)) && p.wants_list(Some(0)) && p.wants_list(None));
		let s = p.conclude(Walked::unchanged(String::new(), Some(1), None), 200, now());
		assert_eq!((s.reviews.len(), s.coverage, s.cut_after, s.listed), (0, Coverage::DownTo(None), None, Some(1)));
		assert!(s.warnings.is_empty());

		let owed = [
			Known {
				listed: Some(1),
				..known(["a".to_owned()], &["a"])
			},
			Known {
				cut_after: Some("a".into()),
				..settled.clone()
			},
			Known { initial: true, ..settled.clone() },
		];
		for k in &owed {
			assert!(NewestFirst::new(k, now()).wants_list(Some(1)), "{k:?}");
		}
		assert!(Requested::new(&settled, None::<Vec<String>>).wants_list(Some(1)));
	}

	#[test]
	fn requested_named_stops_when_all_are_found() {
		let none = Known::default();
		let mut p = Requested::new(&none, Some(["b".to_owned()]));
		assert!(!p.wants_capture(&card("a", "").0) && p.wants_capture(&card("b", "").0));
		p.observe(&card("a", "").0);
		assert!(!p.satisfied());
		p.observe(&card("b", "").0);
		assert!(p.satisfied());
		assert!(!Requested::new(&none, None::<Vec<String>>).satisfied());
	}
}
