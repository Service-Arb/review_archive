//! A scan against what is already archived: what is new, what changed, what is gone.

use std::collections::HashSet;

use super::{Coverage, Known, Observed, ReviewId, Scan};

#[derive(Debug, Default)]
pub struct Plan<'a> {
	pub new: Vec<&'a Observed>,
	/// Content differs from the archived version: a new `review_versions` row.
	pub changed: Vec<(ReviewId, &'a Observed)>,
	pub unchanged: Vec<(ReviewId, &'a Observed)>,
	/// Was marked gone, is listed again. Also in `changed` or `unchanged`.
	pub reappeared: Vec<ReviewId>,
	/// Newly gone: known, not gone before, and absent where the scan looked.
	pub gone: Vec<ReviewId>,
}

impl Plan<'_> {
	pub fn seen(&self) -> usize {
		self.new.len() + self.changed.len() + self.unchanged.len()
	}
}

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

	for (source_id, k) in &known.reviews {
		if k.gone || seen.contains(source_id.as_str()) {
			continue;
		}
		let covered = match scan.coverage {
			Coverage::Complete => true,
			// Strictly newer: relative dates are coarse ("a year ago" spans twelve months), so a
			// review dated the same as the oldest one seen may simply not have been reached.
			Coverage::DownTo(Some(oldest)) => k.published_est.is_some_and(|est| est > oldest),
			Coverage::DownTo(None) => false,
		};
		if covered {
			plan.gone.push(k.id);
		}
	}
	plan.gone.sort();
	plan
}

#[cfg(test)]
mod tests {
	use jiff::Timestamp;

	use super::*;
	use crate::domain::KnownReview;

	fn obs(id: &str, text: &str) -> Observed {
		Observed {
			source_review_id: id.into(),
			author: "A".into(),
			rating: Some(5),
			text: Some(text.into()),
			..Default::default()
		}
	}

	fn known(entries: &[(&str, i64, &str, bool, Option<&str>)]) -> Known {
		Known {
			reviews: entries
				.iter()
				.map(|&(sid, id, text, gone, est)| {
					(
						sid.to_owned(),
						KnownReview {
							id: ReviewId(id),
							content_hash: obs(sid, text).content_hash(),
							capture_pending: false,
							gone,
							published_est: est.map(|e| e.parse().unwrap()),
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
			("newer", 1, "x", false, Some("2026-09-01T00:00:00Z")),
			("same_day", 2, "x", false, Some("2026-06-01T00:00:00Z")),
			("older", 3, "x", false, Some("2025-01-01T00:00:00Z")),
			("undated", 4, "x", false, None),
		]);
		let scan = Scan {
			reviews: vec![],
			coverage: Coverage::DownTo(Some(ts("2026-06-01T00:00:00Z"))),
			warnings: vec![],
		};
		assert_eq!(plan(&k, &scan).gone, [ReviewId(1)]);

		let undated_walk = Scan {
			coverage: Coverage::DownTo(None),
			..scan
		};
		assert!(plan(&k, &undated_walk).gone.is_empty());
	}
}
