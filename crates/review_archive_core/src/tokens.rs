//! Risk tokens: what a walk costs the signed-in account, and who pays for it. Balances are a
//! ledger's sum; this is the arithmetic, the ledger is the store's.

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;

/// How members' balances renew, and the account's own limit.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, SmartDefault)]
#[cfg_attr(feature = "settings", derive(schemars::JsonSchema, v_utils::macros::SettingsNested))]
#[serde(default, deny_unknown_fields)]
pub struct Tokens {
	/// Added to a member's balance per day, while it is under `cap`.
	#[default(15)]
	pub daily: i64,
	/// Renewal stops here; tokens granted or bought above it stay.
	#[default(300)]
	pub cap: i64,
	/// Spent by all walks together in any hour at most, whoever pays: Maps closes until older walks leave the window.
	#[default(120)]
	pub per_hour: i64,
}

/// What renewal adds to `balance` at `now`, renewed last at `last`, and the time it is renewed
/// up to: whole days only, so asking twice in a day adds once.
pub fn accrue(balance: i64, last: Timestamp, now: Timestamp, daily: i64, cap: i64) -> (i64, Timestamp) {
	let days = now.duration_since(last).as_secs() / 86_400;
	if days <= 0 {
		return (0, last);
	}
	let delta = (days * daily).min(cap - balance).max(0);
	(delta, last + SignedDuration::from_hours(24 * days))
}

/// `cost` over `payers` (`(who, balance)`): equal shares, none past its payer's balance, what
/// one cannot pay going to the others. A cost past every balance together is charged up to them.
pub fn split<W: Clone>(cost: i64, payers: &[(W, i64)]) -> Vec<(W, i64)> {
	assert!(cost >= 0 && payers.iter().all(|(_, b)| *b >= 0), "costs and balances are never negative");
	let mut by_balance: Vec<&(W, i64)> = payers.iter().collect();
	by_balance.sort_by_key(|(_, b)| *b);
	let mut left = cost;
	let n = by_balance.len() as i64;
	by_balance
		.into_iter()
		.zip(0..)
		.map(|((who, balance), i)| {
			let share = (left + (n - i) - 1) / (n - i);
			let paid = share.min(*balance);
			left -= paid;
			(who.clone(), paid)
		})
		.filter(|(_, paid)| *paid > 0)
		.collect()
}

/// What a walk may spend, and has.
#[derive(Debug)]
pub struct Meter {
	allowance: i64,
	spent: i64,
}

impl Meter {
	/// A walk that may spend `allowance`.
	pub fn new(allowance: i64) -> Self {
		Self { allowance, spent: 0 }
	}

	/// Spends `cost`, whatever is left: the steps a walk cannot do without.
	pub fn spend(&mut self, cost: i64) {
		self.spent += cost;
	}

	/// Spends `cost` if it fits; `false`: the walk is at its budget.
	pub fn try_spend(&mut self, cost: i64) -> bool {
		let fits = self.spent + cost <= self.allowance;
		if fits {
			self.spent += cost;
		}
		fits
	}

	/// What it may still spend.
	pub fn left(&self) -> i64 {
		self.allowance - self.spent
	}

	/// Spent so far.
	pub fn spent(&self) -> i64 {
		self.spent
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn day(n: i64) -> Timestamp {
		Timestamp::from_second(1_790_000_000 + n * 86_400).unwrap()
	}

	#[test]
	fn renewal_is_by_whole_days_up_to_the_cap() {
		assert_eq!(accrue(0, day(0), day(1) - SignedDuration::from_secs(1), 15, 300), (0, day(0)));
		assert_eq!(accrue(0, day(0), day(3) + SignedDuration::from_hours(5), 15, 300), (45, day(3)));
		assert_eq!(accrue(290, day(0), day(30), 15, 300), (10, day(30)));
		assert_eq!(accrue(500, day(0), day(2), 15, 300), (0, day(2)), "bought tokens above the cap stay");
	}

	#[test]
	fn the_poorer_payer_pays_what_it_has_and_the_rest_falls_on_the_others() {
		let payers = |bs: &[i64]| bs.iter().enumerate().map(|(i, b)| (format!("m{i}"), *b)).collect::<Vec<_>>();
		assert_eq!(split(20, &payers(&[5, 100])), [("m0".to_owned(), 5), ("m1".to_owned(), 15)]);
		assert_eq!(split(21, &payers(&[100, 100, 100])).iter().map(|(_, p)| p).sum::<i64>(), 21);
		assert_eq!(split(50, &payers(&[10, 20])), [("m0".to_owned(), 10), ("m1".to_owned(), 20)], "past every balance");
		assert!(split(0, &payers(&[10])).is_empty());
	}
}
