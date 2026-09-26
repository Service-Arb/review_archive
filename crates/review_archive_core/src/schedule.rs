//! When a target is next due. Pure: the caller brings the time and the randomness.

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};

use crate::TargetId;

/// How often a target is scanned unless told otherwise.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// No target is scanned more often than this.
pub const MIN_INTERVAL: Duration = Duration::from_secs(3600);
/// Relative spread applied to every delay, so targets added together drift apart.
pub const JITTER: f64 = 0.10;
/// The first retry after a failure; doubles per consecutive failure.
pub const BACKOFF_BASE: Duration = Duration::from_secs(3600);
/// The longest a failing target waits.
pub const BACKOFF_CAP: Duration = Duration::from_secs(24 * 3600);
/// Shortest pause between two targets of one pass.
pub const PAUSE_MIN: Duration = Duration::from_secs(5);
/// Longest pause between two targets of one pass.
pub const PAUSE_MAX: Duration = Duration::from_secs(15);

/// The part of a target's run history scheduling depends on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LastRun {
	/// When it ended.
	pub finished_at: Timestamp,
	/// Failed runs since the last one that was not.
	pub consecutive_failures: u32,
}

/// The delay after the last run, before jitter.
pub fn base_delay(interval: Duration, consecutive_failures: u32) -> Duration {
	if consecutive_failures == 0 {
		return interval;
	}
	let factor = 2u32.saturating_pow(consecutive_failures - 1);
	BACKOFF_BASE.saturating_mul(factor).min(BACKOFF_CAP)
}

/// When the target is next due; `None` for a target never run, which is due now.
///
/// The jitter is derived from the target and the run it follows rather than drawn
/// fresh, so asking twice gives the same answer — a scheduler that re-rolls on every
/// wake-up would drift every target towards its earliest possible time.
pub fn due_at(target: TargetId, interval: Duration, last: Option<LastRun>) -> Option<Timestamp> {
	let last = last?;
	let seed = (target.0 as u64) ^ (last.finished_at.as_second() as u64).rotate_left(17);
	let mut delay = jittered(base_delay(interval, last.consecutive_failures), unit_noise(seed));
	if last.consecutive_failures > 0 {
		delay = delay.min(BACKOFF_CAP);
	}
	let delay = SignedDuration::try_from(delay).unwrap_or(SignedDuration::MAX);
	Some(last.finished_at.checked_add(delay).unwrap_or(Timestamp::MAX))
}

/// `delay` scaled by `1 + JITTER * u`, for `u` in `[-1, 1]`.
pub fn jittered(delay: Duration, u: f64) -> Duration {
	delay.mul_f64(1.0 + JITTER * u.clamp(-1.0, 1.0))
}

/// The pause between two targets in one pass, for `u` in `[0, 1]`.
pub fn pause(u: f64) -> Duration {
	PAUSE_MIN + (PAUSE_MAX - PAUSE_MIN).mul_f64(u.clamp(0.0, 1.0))
}

/// A well-mixed value in `[-1, 1]` from a seed (splitmix64).
pub fn unit_noise(seed: u64) -> f64 {
	let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
	z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
	z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
	z ^= z >> 31;
	(z >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
	use super::*;

	const H: Duration = Duration::from_secs(3600);

	#[test]
	fn never_run_is_due_now() {
		assert_eq!(due_at(TargetId(1), DEFAULT_INTERVAL, None), None);
	}

	#[test]
	fn interval_with_jitter_bounds() {
		let finished: Timestamp = "2026-09-26T00:00:00Z".parse().unwrap();
		let mut spread = (i64::MAX, i64::MIN);
		for id in 0..500 {
			let due = due_at(
				TargetId(id),
				6 * H,
				Some(LastRun {
					finished_at: finished,
					consecutive_failures: 0,
				}),
			)
			.unwrap();
			let secs = (due - finished).get_seconds();
			assert!((5 * 3600 + 1440..=6 * 3600 + 2160).contains(&secs), "{secs}s is outside 6h ± 10%");
			spread = (spread.0.min(secs), spread.1.max(secs));
		}
		// the jitter actually spreads, rather than sitting at one end
		assert!(spread.0 < 6 * 3600 - 1800 && spread.1 > 6 * 3600 + 1800, "{spread:?}");
	}

	#[test]
	fn same_question_same_answer() {
		let last = Some(LastRun {
			finished_at: "2026-09-26T00:00:00Z".parse().unwrap(),
			consecutive_failures: 0,
		});
		assert_eq!(due_at(TargetId(7), 6 * H, last), due_at(TargetId(7), 6 * H, last));
	}

	#[test]
	fn backoff_doubles_and_caps() {
		assert_eq!(base_delay(6 * H, 0), 6 * H);
		assert_eq!(base_delay(6 * H, 1), H);
		assert_eq!(base_delay(6 * H, 2), 2 * H);
		assert_eq!(base_delay(6 * H, 4), 8 * H);
		assert_eq!(base_delay(6 * H, 5), 16 * H);
		assert_eq!(base_delay(6 * H, 6), 24 * H);
		assert_eq!(base_delay(6 * H, 60), 24 * H);

		// jitter does not push a capped backoff past the cap
		let finished: Timestamp = "2026-09-26T00:00:00Z".parse().unwrap();
		for id in 0..200 {
			let due = due_at(
				TargetId(id),
				6 * H,
				Some(LastRun {
					finished_at: finished,
					consecutive_failures: 9,
				}),
			)
			.unwrap();
			assert!((due - finished).get_seconds() <= 24 * 3600);
		}
	}

	#[test]
	fn jitter_and_pause_bounds() {
		assert_eq!(jittered(10 * H, 1.0), 11 * H);
		assert_eq!(jittered(10 * H, -1.0), 9 * H);
		assert_eq!(jittered(10 * H, 7.0), 11 * H);
		assert_eq!(pause(0.0), PAUSE_MIN);
		assert_eq!(pause(1.0), PAUSE_MAX);
		for seed in 0..10_000 {
			assert!((-1.0..=1.0).contains(&unit_noise(seed)));
		}
	}
}
