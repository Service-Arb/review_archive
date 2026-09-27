//! Relative dates the inline tests leave out: the singular, article-only forms of every
//! language the scanner runs in, minutes, and "just now".

use jiff::Timestamp;
use review_archive_core::relative_date::{estimate, lower_bound};

fn now() -> Timestamp {
	"2026-09-26T12:00:00Z".parse().unwrap()
}

fn at(raw: &str) -> Option<String> {
	estimate(raw, now()).map(|t| t.strftime("%Y-%m-%dT%H:%M").to_string())
}

#[test]
fn an_article_means_one_unit_in_every_language() {
	assert_eq!(at("a week ago").as_deref(), Some("2026-09-19T12:00"));
	assert_eq!(at("a day ago").as_deref(), Some("2026-09-25T12:00"));
	assert_eq!(at("vor einer Stunde").as_deref(), Some("2026-09-26T11:00"));
	assert_eq!(at("un'ora fa").as_deref(), Some("2026-09-26T11:00"));
	assert_eq!(at("una settimana fa").as_deref(), Some("2026-09-19T12:00"));
	assert_eq!(at("hace una hora").as_deref(), Some("2026-09-26T11:00"));
	assert_eq!(at("il y a un mois").as_deref(), Some("2026-08-26T12:00"));
}

#[test]
fn plural_days_and_minutes_in_every_language() {
	assert_eq!(at("3 giorni fa").as_deref(), Some("2026-09-23T12:00"));
	assert_eq!(at("hace 2 días").as_deref(), Some("2026-09-24T12:00"));
	assert_eq!(at("vor 3 Tagen").as_deref(), Some("2026-09-23T12:00"));
	assert_eq!(at("il y a 5 minutes").as_deref(), Some("2026-09-26T11:55"));
	assert_eq!(at("vor 5 Minuten").as_deref(), Some("2026-09-26T11:55"));
	assert_eq!(at("5 minuti fa").as_deref(), Some("2026-09-26T11:55"));
	assert_eq!(at("hace 5 minutos").as_deref(), Some("2026-09-26T11:55"));
}

#[test]
fn edited_prefixes_are_read_past_in_every_language() {
	assert_eq!(at("Bearbeitet: vor 2 Wochen").as_deref(), Some("2026-09-12T12:00"));
	assert_eq!(at("Modificato 3 settimane fa").as_deref(), Some("2026-09-05T12:00"));
	assert_eq!(at("Editado hace 2 años").as_deref(), Some("2024-09-26T12:00"));
}

#[test]
fn just_now_is_now_and_at_most_a_minute_old() {
	assert_eq!(estimate("à l'instant", now()), Some(now()));
	assert_eq!(lower_bound("Just now", now()).map(|t| t.to_string()).as_deref(), Some("2026-09-26T11:59:00Z"));
}
