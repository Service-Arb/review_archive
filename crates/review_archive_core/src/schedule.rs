//! When a target is next due. Pure: the caller brings the time and the randomness.

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;
use v_utils::Timeframe;

use crate::TargetId;

/// How targets are paced.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, SmartDefault)]
#[cfg_attr(feature = "settings", derive(schemars::JsonSchema, v_utils::macros::SettingsNested))]
#[serde(default, deny_unknown_fields)]
pub struct Schedule {
	/// No target is scanned more often than this.
	#[default(Timeframe::from("1h"))]
	pub min_interval: Timeframe,
	/// Relative spread applied to every delay, so targets added together drift apart.
	#[default(0.10)]
	pub jitter: f64,
	/// The first wait after a failure, of a target or of the Maps breaker; doubles per failure in a row.
	#[default(Timeframe::from("1h"))]
	pub backoff_base: Timeframe,
	/// The longest a failing target, or a tripped breaker, waits.
	#[default(Timeframe::from("1d"))]
	pub backoff_cap: Timeframe,
	/// Shortest pause between two scans.
	#[default(Timeframe::from("5s"))]
	pub pause_min: Timeframe,
	/// Longest pause between two scans.
	#[default(Timeframe::from("15s"))]
	pub pause_max: Timeframe,
}

impl Schedule {
	/// The delay after the last run, before jitter.
	fn base_delay(&self, interval: Timeframe, consecutive_failures: u32) -> SignedDuration {
		if consecutive_failures == 0 {
			return span(interval);
		}
		backoff(self.backoff_base, self.backoff_cap, consecutive_failures)
	}

	/// When the target is next due; `None` for a target never run, which is due now.
	///
	/// The jitter is derived from the target and the run it follows rather than drawn
	/// fresh, so asking twice gives the same answer — a scheduler that re-rolls on every
	/// wake-up would drift every target towards its earliest possible time.
	pub fn due_at(&self, target: TargetId, interval: Timeframe, last: Option<LastRun>) -> Option<Timestamp> {
		let last = last?;
		let seed = (target.0 as u64) ^ (last.finished_at.as_second() as u64).rotate_left(17);
		let mut delay = self.jittered(self.base_delay(interval, last.consecutive_failures), unit_noise(seed));
		if last.consecutive_failures > 0 {
			delay = delay.min(span(self.backoff_cap));
		}
		Some(last.finished_at.checked_add(delay).unwrap_or(Timestamp::MAX))
	}

	/// `delay` scaled by `1 + jitter * u`, for `u` in `[-1, 1]`.
	fn jittered(&self, delay: SignedDuration, u: f64) -> SignedDuration {
		delay.mul_f64(1.0 + self.jitter * u.clamp(-1.0, 1.0))
	}

	/// The pause between two scans, for `u` in `[0, 1]`.
	pub fn pause(&self, u: f64) -> std::time::Duration {
		let (min, max) = (self.pause_min.duration(), self.pause_max.duration());
		min + (max - min).mul_f64(u.clamp(0.0, 1.0))
	}
}

/// The part of a target's run history scheduling depends on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LastRun {
	/// When it ended.
	pub finished_at: Timestamp,
	/// Failed runs since the last one that was not.
	pub consecutive_failures: u32,
}

/// The wait after the `n`th failure in a row (1-based): `base`, doubling, at most `cap`.
pub fn backoff(base: Timeframe, cap: Timeframe, n: u32) -> SignedDuration {
	span(base).saturating_mul(2i32.saturating_pow(n.saturating_sub(1))).min(span(cap))
}

fn span(t: Timeframe) -> SignedDuration {
	SignedDuration::try_from(t.duration()).expect("a Timeframe is at most u64::MAX ms, well within SignedDuration")
}

/// Google flagged the address we scan from: every Maps walk pauses, whatever the
/// target, until `probe_after`, when one scan probes. Per-target backoff can't express
/// this — the block is not the target's.
#[derive(Clone, Debug, PartialEq)]
pub struct Breaker {
	/// The first trip of this streak.
	pub tripped_at: Timestamp,
	/// The diagnostic code of the last failure that tripped it.
	pub reason: String,
	/// Trips in a row, the failed probes included.
	pub trips: u32,
	/// When one scan may try again.
	pub probe_after: Timestamp,
}

impl Breaker {
	/// Trips a closed breaker, or trips again after a failed probe: the wait doubles, base to cap.
	pub fn trip(prev: Option<Self>, reason: &str, now: Timestamp, schedule: &Schedule) -> Self {
		let trips = prev.as_ref().map_or(1, |b| b.trips + 1);
		Self {
			tripped_at: prev.map_or(now, |b| b.tripped_at),
			reason: reason.to_owned(),
			trips,
			probe_after: now + backoff(schedule.backoff_base, schedule.backoff_cap, trips),
		}
	}
}

/// A well-mixed value in `[-1, 1]` from a seed (splitmix64).
fn unit_noise(seed: u64) -> f64 {
	let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
	z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
	z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
	z ^= z >> 31;
	(z >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
	use v_utils::{TF_1D, TF_6H};

	use super::*;

	const H: SignedDuration = SignedDuration::from_hours(1);

	fn after(finished_at: Timestamp, consecutive_failures: u32) -> Option<LastRun> {
		Some(LastRun { finished_at, consecutive_failures })
	}

	#[test]
	fn never_run_is_due_now() {
		assert_eq!(Schedule::default().due_at(TargetId(1), TF_1D, None), None);
	}

	#[test]
	fn interval_with_jitter_bounds() {
		let s = Schedule::default();
		let finished: Timestamp = "2026-09-26T00:00:00Z".parse().unwrap();
		let mut spread = (i64::MAX, i64::MIN);
		for id in 0..500 {
			let due = s.due_at(TargetId(id), TF_6H, after(finished, 0)).unwrap();
			let secs = due.duration_since(finished).as_secs();
			assert!((5 * 3600 + 1440..=6 * 3600 + 2160).contains(&secs), "{secs}s is outside 6h ± 10%");
			spread = (spread.0.min(secs), spread.1.max(secs));
		}
		// the jitter actually spreads, rather than sitting at one end
		assert!(spread.0 < 6 * 3600 - 1800 && spread.1 > 6 * 3600 + 1800, "{spread:?}");
	}

	#[test]
	fn same_question_same_answer() {
		let s = Schedule::default();
		let last = after("2026-09-26T00:00:00Z".parse().unwrap(), 0);
		assert_eq!(s.due_at(TargetId(7), TF_6H, last), s.due_at(TargetId(7), TF_6H, last));
	}

	#[test]
	fn backoff_doubles_and_caps() {
		let s = Schedule::default();
		assert_eq!(s.base_delay(TF_6H, 0), 6 * H);
		assert_eq!(s.base_delay(TF_6H, 1), H);
		assert_eq!(s.base_delay(TF_6H, 2), 2 * H);
		assert_eq!(s.base_delay(TF_6H, 4), 8 * H);
		assert_eq!(s.base_delay(TF_6H, 5), 16 * H);
		assert_eq!(s.base_delay(TF_6H, 6), 24 * H);
		assert_eq!(s.base_delay(TF_6H, 60), 24 * H);

		// jitter does not push a capped backoff past the cap
		let finished: Timestamp = "2026-09-26T00:00:00Z".parse().unwrap();
		for id in 0..200 {
			let due = s.due_at(TargetId(id), TF_6H, after(finished, 9)).unwrap();
			assert!(due.duration_since(finished) <= 24 * H);
		}
	}

	#[test]
	fn breaker_waits_double_from_the_last_trip() {
		let s = Schedule::default();
		let t0: Timestamp = "2026-09-26T00:00:00Z".parse().unwrap();
		let first = Breaker::trip(None, "blocked", t0, &s);
		assert_eq!((first.trips, first.probe_after), (1, t0 + H));
		let probe_failed = first.probe_after + SignedDuration::from_mins(5);
		let second = Breaker::trip(Some(first), "limited_view", probe_failed, &s);
		assert_eq!((second.tripped_at, second.trips, second.reason.as_str()), (t0, 2, "limited_view"));
		assert_eq!(second.probe_after, probe_failed + 2 * H);
		let many = (0..10).fold(second, |b, _| Breaker::trip(Some(b), "blocked", t0, &s));
		assert_eq!(many.probe_after, t0 + 24 * H);
	}

	#[test]
	fn jitter_and_pause_bounds() {
		let s = Schedule::default();
		assert_eq!(s.jittered(10 * H, 1.0), 11 * H);
		assert_eq!(s.jittered(10 * H, -1.0), 9 * H);
		assert_eq!(s.jittered(10 * H, 7.0), 11 * H);
		assert_eq!(s.pause(0.0), s.pause_min.duration());
		assert_eq!(s.pause(1.0), s.pause_max.duration());
		for seed in 0..10_000 {
			assert!((-1.0..=1.0).contains(&unit_noise(seed)));
		}
	}
}
