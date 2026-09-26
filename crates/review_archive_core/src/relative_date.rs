//! Maps shows when a review was posted only as "3 weeks ago", in the page's language.
//! This turns that into a point in time, as precise as the phrase is.

use jiff::{Span, Timestamp, tz::TimeZone};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Unit {
	Minute,
	Hour,
	Day,
	Week,
	Month,
	Year,
}

/// Word stems per unit across the languages the scanner is run in (en, fr, de, es, it).
/// Matched against whole lowercase tokens by prefix, except the entries in `EXACT`.
const STEMS: &[(Unit, &[&str])] = &[
	(Unit::Minute, &["minute", "minuto", "min"]),
	(Unit::Hour, &["hour", "heure", "stunde", "hora", "ora", "ore"]),
	(Unit::Day, &["day", "jour", "tag", "día", "dia", "giorn"]),
	(Unit::Week, &["week", "semaine", "woche", "semana", "settiman"]),
	(Unit::Month, &["month", "mois", "monat", "mes", "mese"]),
	(Unit::Year, &["year", "jahr", "año", "ano", "ann"]),
];

/// Too short to match by prefix without catching unrelated words.
const EXACT: &[(Unit, &[&str])] = &[(Unit::Year, &["an", "ans"])];

const YESTERDAY: &[&str] = &["yesterday", "hier", "gestern", "ayer", "ieri"];
const JUST_NOW: &[&str] = &["now", "instant", "jetzt", "ahora", "adesso"];

/// Estimates when a review was published from its relative date text, or `None`
/// when the phrase is not one this recognises.
///
/// The estimate is the *latest* moment the phrase allows: "3 weeks ago" is said of
/// anything from three up to (not including) four weeks back, so the review is at most
/// one unit older than this. [`lower_bound`] gives the other end.
pub fn estimate(raw: &str, now: Timestamp) -> Option<Timestamp> {
	match phrase(raw)? {
		Phrase::Now => Some(now),
		Phrase::Ago(unit, n) => shift(now, unit, n),
	}
}

/// The earliest moment a review estimated at `est` from `raw` can have been published:
/// `est` less one unit of the phrase. An absolute timestamp (what the Business Profile
/// API gives) is exact. `None` when `raw` is neither.
pub fn lower_bound(raw: &str, est: Timestamp) -> Option<Timestamp> {
	if raw.parse::<Timestamp>().is_ok() {
		return Some(est);
	}
	match phrase(raw)? {
		Phrase::Now => shift(est, Unit::Minute, 1),
		Phrase::Ago(unit, _) => shift(est, unit, 1),
	}
}

enum Phrase {
	Now,
	Ago(Unit, i64),
}

fn phrase(raw: &str) -> Option<Phrase> {
	let lower = raw.to_lowercase();
	let tokens: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();

	if tokens.iter().any(|t| YESTERDAY.contains(t)) {
		return Some(Phrase::Ago(Unit::Day, 1));
	}
	// stems first: the English article "an" ("an hour ago") is also French for a year
	let unit = tokens.iter().find_map(|t| stem_unit(t)).or_else(|| tokens.iter().find_map(|t| exact_unit(t)));
	let Some(unit) = unit else {
		return tokens.iter().any(|t| JUST_NOW.contains(t)).then_some(Phrase::Now);
	};
	// No digits means an article: "a week ago", "il y a un mois", "vor einem Jahr".
	let n = tokens.iter().find_map(|t| t.parse::<i64>().ok()).unwrap_or(1);
	Some(Phrase::Ago(unit, n))
}

fn stem_unit(token: &str) -> Option<Unit> {
	STEMS.iter().find(|(_, stems)| stems.iter().any(|s| token.starts_with(s))).map(|(u, _)| *u)
}

fn exact_unit(token: &str) -> Option<Unit> {
	EXACT.iter().find(|(_, words)| words.contains(&token)).map(|(u, _)| *u)
}

fn shift(now: Timestamp, unit: Unit, n: i64) -> Option<Timestamp> {
	let span = match unit {
		Unit::Minute => Span::new().try_minutes(n),
		Unit::Hour => Span::new().try_hours(n),
		Unit::Day => Span::new().try_days(n),
		Unit::Week => Span::new().try_weeks(n),
		Unit::Month => Span::new().try_months(n),
		Unit::Year => Span::new().try_years(n),
	}
	.ok()?;
	// calendar units need a calendar; UTC is as good as any for a phrase this coarse
	now.to_zoned(TimeZone::UTC).checked_sub(span).ok().map(|z| z.timestamp())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(raw: &str) -> Option<String> {
		let now: Timestamp = "2026-09-26T12:00:00Z".parse().unwrap();
		estimate(raw, now).map(|t| t.strftime("%Y-%m-%dT%H:%M").to_string())
	}

	#[test]
	fn english() {
		assert_eq!(at("3 weeks ago").as_deref(), Some("2026-09-05T12:00"));
		assert_eq!(at("a month ago").as_deref(), Some("2026-08-26T12:00"));
		assert_eq!(at("2 years ago").as_deref(), Some("2024-09-26T12:00"));
		assert_eq!(at("an hour ago").as_deref(), Some("2026-09-26T11:00"));
		assert_eq!(at("Edited 5 days ago").as_deref(), Some("2026-09-21T12:00"));
		assert_eq!(at("yesterday").as_deref(), Some("2026-09-25T12:00"));
	}

	#[test]
	fn french() {
		assert_eq!(at("il y a 3 semaines").as_deref(), Some("2026-09-05T12:00"));
		assert_eq!(at("il y a un an").as_deref(), Some("2025-09-26T12:00"));
		assert_eq!(at("il y a 2 ans").as_deref(), Some("2024-09-26T12:00"));
		assert_eq!(at("il y a 4 mois").as_deref(), Some("2026-05-26T12:00"));
		assert_eq!(at("Modifié il y a une semaine").as_deref(), Some("2026-09-19T12:00"));
	}

	#[test]
	fn german_spanish_italian() {
		assert_eq!(at("vor 2 Wochen").as_deref(), Some("2026-09-12T12:00"));
		assert_eq!(at("vor einem Jahr").as_deref(), Some("2025-09-26T12:00"));
		assert_eq!(at("hace 3 meses").as_deref(), Some("2026-06-26T12:00"));
		assert_eq!(at("hace un año").as_deref(), Some("2025-09-26T12:00"));
		assert_eq!(at("2 mesi fa").as_deref(), Some("2026-07-26T12:00"));
	}

	#[test]
	fn lower_bound_is_one_unit_earlier() {
		let now: Timestamp = "2026-09-26T12:00:00Z".parse().unwrap();
		let lb = |raw: &str| {
			let est = estimate(raw, now).unwrap();
			lower_bound(raw, est).map(|t| t.strftime("%Y-%m-%dT%H:%M").to_string())
		};
		assert_eq!(lb("a month ago").as_deref(), Some("2026-07-26T12:00"));
		assert_eq!(lb("il y a 3 semaines").as_deref(), Some("2026-08-29T12:00"));
		assert_eq!(lb("yesterday").as_deref(), Some("2026-09-24T12:00"));
		let exact: Timestamp = "2026-09-01T10:00:00Z".parse().unwrap();
		assert_eq!(lower_bound("2026-09-01T10:00:00Z", exact), Some(exact));
		assert_eq!(lower_bound("NEW", exact), None);
	}

	#[test]
	fn unknown_is_none() {
		assert_eq!(at(""), None);
		assert_eq!(at("NEW"), None);
	}
}
