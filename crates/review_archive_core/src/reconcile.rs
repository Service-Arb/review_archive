//! A scan against what is already archived: what is new, what changed, what is gone.

use std::collections::HashSet;

use jiff::Timestamp;

use crate::{Coverage, Known, KnownReview, Observed, ReviewId, Scan, relative_date};

/// What to write for one scan.
#[derive(Debug, Default)]
pub struct Plan<'a> {
	/// Not archived before.
	pub new: Vec<&'a Observed>,
	/// Content differs from the archived version: a new `review_versions` row.
	pub changed: Vec<(ReviewId, &'a Observed)>,
	/// Archived, and the same.
	pub unchanged: Vec<(ReviewId, &'a Observed)>,
	/// Was marked gone, is listed again. Also in `changed` or `unchanged`.
	pub reappeared: Vec<ReviewId>,
	/// Newly gone: known, not gone before, and absent where the scan looked.
	pub gone: Vec<ReviewId>,
	/// Why the scan was not trusted as far as its coverage claimed. Makes the run `partial`.
	pub warnings: Vec<String>,
}

impl Plan<'_> {
	/// Distinct reviews the scan listed.
	pub fn seen(&self) -> usize {
		self.new.len() + self.changed.len() + self.unchanged.len()
	}
}

/// Compares a scan with what is archived. Only reviews the scan's coverage reaches can be
/// judged gone; see [`Coverage`].
pub fn plan<'a>(known: &Known, scan: &'a Scan) -> Plan<'a> {
	let mut plan = Plan::default();
	let mut seen = HashSet::new();
	for obs in &scan.reviews {
		// the same card can be read twice across scroll steps; the first reading wins
		if !seen.insert(obs.source_review_id.as_str()) {
			continue;
		}
		match known.reviews.get(&obs.source_review_id) {
			None => plan.new.push(obs),
			Some(k) => {
				if k.gone {
					plan.reappeared.push(k.id);
				}
				if k.content_hash == obs.content_hash() {
					plan.unchanged.push((k.id, obs));
				} else {
					plan.changed.push((k.id, obs));
				}
			}
		}
	}

	let live_known = known.reviews.values().any(|k| !k.gone);
	if scan.coverage == Coverage::Complete && seen.is_empty() && live_known {
		plan.warnings
			.push("the source listed no reviews at all while the archive has live ones; not taking that as every review gone".to_owned());
		return plan;
	}

	for (source_id, k) in &known.reviews {
		if k.gone || seen.contains(source_id.as_str()) {
			continue;
		}
		let covered = match scan.coverage {
			Coverage::Complete => true,
			// The walk saw a newest-first prefix of the list, down to a card estimated at
			// `oldest` — which is the latest that card can be. A known review was in that prefix
			// only if even the earliest it can be is no earlier: estimates are coarse ("a month
			// ago" spans a month), and the one on record was made on an earlier day.
			Coverage::DownTo(Some(oldest)) => earliest(k).is_some_and(|lo| lo >= oldest),
			Coverage::DownTo(None) => false,
		};
		if covered {
			plan.gone.push(k.id);
		}
	}
	plan.gone.sort();
	plan
}

/// The earliest a known review can have been published; `None` when that is unknowable.
fn earliest(k: &KnownReview) -> Option<Timestamp> {
	relative_date::lower_bound(k.published_raw.as_deref()?, k.published_est?)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn obs(id: &str, text: &str) -> Observed {
		Observed {
			source_review_id: id.into(),
			author: "A".into(),
			rating: Some(5),
			text: Some(text.into()),
			..Default::default()
		}
	}

	/// `(source id, id, text, gone, (published_raw, published_est))`
	type Entry<'a> = (&'a str, i64, &'a str, bool, Option<(&'a str, &'a str)>);

	fn known(entries: &[Entry<'_>]) -> Known {
		Known {
			reviews: entries
				.iter()
				.map(|&(sid, id, text, gone, date)| {
					(
						sid.to_owned(),
						KnownReview {
							id: ReviewId(id),
							content_hash: obs(sid, text).content_hash(),
							capture_pending: false,
							gone,
							published_est: date.map(|(_, e)| e.parse().unwrap()),
							published_raw: date.map(|(raw, _)| raw.to_owned()),
							author: "A".into(),
							rating: Some(5),
							text: Some(text.into()),
						},
					)
				})
				.collect(),
		}
	}

	fn ts(s: &str) -> Timestamp {
		s.parse().unwrap()
	}

	#[test]
	fn new_changed_unchanged_reappeared() {
		let k = known(&[("a", 1, "same", false, None), ("b", 2, "old", false, None), ("c", 3, "back", true, None)]);
		let scan = Scan {
			reviews: vec![obs("new", "hi"), obs("a", "same"), obs("b", "edited"), obs("c", "back"), obs("a", "same")],
			coverage: Coverage::DownTo(None),
			warnings: vec![],
		};
		let p = plan(&k, &scan);
		assert_eq!(p.new.iter().map(|o| o.source_review_id.as_str()).collect::<Vec<_>>(), ["new"]);
		assert_eq!(p.changed.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [ReviewId(2)]);
		assert_eq!(p.unchanged.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [ReviewId(1), ReviewId(3)]);
		assert_eq!(p.reappeared, [ReviewId(3)]);
		assert!(p.gone.is_empty());
		assert_eq!(p.seen(), 4);
	}

	#[test]
	fn complete_coverage_marks_every_absent_review_gone_once() {
		let k = known(&[("a", 1, "x", false, None), ("b", 2, "x", true, None), ("c", 3, "x", false, None)]);
		let scan = Scan {
			reviews: vec![obs("c", "x")],
			coverage: Coverage::Complete,
			warnings: vec![],
		};
		// b is already gone and stays so without being marked again
		assert_eq!(plan(&k, &scan).gone, [ReviewId(1)]);
	}

	#[test]
	fn partial_walk_only_judges_reviews_it_walked_past() {
		let k = known(&[
			// a week ago on 2026-09-08: surely newer than anything estimated at 2026-08-01
			("newer", 1, "x", false, Some(("a week ago", "2026-09-01T00:00:00Z"))),
			("same_day", 2, "x", false, Some(("2 months ago", "2026-06-01T00:00:00Z"))),
			("older", 3, "x", false, Some(("2 years ago", "2025-01-01T00:00:00Z"))),
			("undated", 4, "x", false, None),
		]);
		let scan = Scan {
			reviews: vec![],
			coverage: Coverage::DownTo(Some(ts("2026-08-01T00:00:00Z"))),
			warnings: vec![],
		};
		assert_eq!(plan(&k, &scan).gone, [ReviewId(1)]);

		let undated_walk = Scan {
			coverage: Coverage::DownTo(None),
			..scan
		};
		assert!(plan(&k, &undated_walk).gone.is_empty());
	}

	/// A review first seen as "a month ago" on 2026-09-01 was estimated at 2026-08-01, but
	/// may be from as early as 2026-07-01. A later walk that stopped at a card estimated at
	/// 2026-07-26 has not necessarily reached it.
	#[test]
	fn a_coarse_estimate_is_not_taken_as_walked_past() {
		let k = known(&[("coarse", 1, "x", false, Some(("a month ago", "2026-08-01T00:00:00Z")))]);
		let scan = Scan {
			reviews: vec![obs("other", "y")],
			coverage: Coverage::DownTo(Some(ts("2026-07-26T12:00:00Z"))),
			warnings: vec![],
		};
		assert!(plan(&k, &scan).gone.is_empty());
		// walked well past even the earliest it can be: gone
		let deeper = Scan {
			coverage: Coverage::DownTo(Some(ts("2026-06-30T00:00:00Z"))),
			..scan
		};
		assert_eq!(plan(&k, &deeper).gone, [ReviewId(1)]);
	}

	/// A "complete" list with nothing in it, against an archive that has live reviews, is far
	/// more likely a broken response than every review deleted at once.
	#[test]
	fn an_empty_complete_scan_marks_nothing_gone() {
		let k = known(&[("a", 1, "x", false, None), ("b", 2, "x", false, None)]);
		let scan = Scan {
			reviews: vec![],
			coverage: Coverage::Complete,
			warnings: vec![],
		};
		let p = plan(&k, &scan);
		assert!(p.gone.is_empty());
		assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);

		// an archive with nothing live has nothing to lose: no warning
		let all_gone = known(&[("a", 1, "x", true, None)]);
		assert!(plan(&all_gone, &scan).warnings.is_empty());
	}
}
