//! One place's reviews in three columns. A card is dragged from Removed to Reinstating when
//! its reinstatement was asked of Google, and back when that is withdrawn; the columns keep
//! their own order, and a review a scan lists again goes back to Snapshotted by itself.

use dioxus::{prelude::*, web::WebEventExt};
use review_archive_client::dto::{BoardCard, GmailDto, LocationSummary};

use crate::{Api, Badge, Refresh, Route, Tone, View, act, ago, shown};

#[derive(Clone, Copy, PartialEq)]
enum Column {
	Snapshotted,
	Removed,
	Reinstating,
}

#[component]
pub fn Board(gmail: GmailDto, location: LocationSummary, tab: Option<String>) -> Element {
	let Api(api) = use_context();
	let Refresh(refresh) = use_context();
	let (g, target) = (gmail.id, location.target.id);
	let board = use_resource(move || {
		let api = api.clone();
		async move {
			refresh();
			api.board(g, target).await.map_err(shown)
		}
	});
	let dragging = use_signal(|| None::<(i64, Column)>);
	let board = match &*board.read() {
		Some(Ok(b)) => b.clone(),
		Some(Err(e)) => return rsx! { div { class: "p-6 text-accent-error", "{e}" } },
		_ => return rsx! { div { class: "p-6 text-ink-soft", "Loading…" } },
	};
	rsx! {
		header { class: "flex h-14 items-center gap-3 border-b border-border px-6",
			Link { class: "text-ink-soft hover:text-ink", to: Route::at(tab, View::Gmail(g)), "{gmail.gmail}" }
			span { class: "text-ink-soft", "/" }
			a {
				class: "font-medium hover:underline",
				href: "https://www.google.com/maps/place/?q=place_id:{location.target.place_id}",
				target: "_blank",
				rel: "noopener noreferrer",
				title: "Open on Google Maps",
				"{location.target.label} ↗"
			}
		}
		main { class: "grid flex-1 grid-cols-3 gap-4 p-6",
			Lane { title: "Snapshotted", column: Column::Snapshotted, cards: board.snapshotted, gmail: g, dragging }
			Lane { title: "Removed", column: Column::Removed, cards: board.removed, gmail: g, dragging }
			Lane { title: "Reinstating", column: Column::Reinstating, cards: board.reinstating, gmail: g, dragging }
		}
	}
}

/// What dropping a card from `from` onto `to` means, if anything.
fn drop_means(from: Column, to: Column) -> Option<bool> {
	match (from, to) {
		(Column::Removed, Column::Reinstating) => Some(true),
		(Column::Reinstating, Column::Removed) => Some(false),
		_ => None,
	}
}

#[component]
fn Lane(title: &'static str, column: Column, cards: Vec<BoardCard>, gmail: i64, dragging: Signal<Option<(i64, Column)>>) -> Element {
	let accepts = dragging().is_some_and(|(_, from)| drop_means(from, column).is_some());
	let frame = if accepts {
		"border-dashed border-primary-ink bg-primary/5"
	} else {
		"border-border bg-secondary"
	};
	rsx! {
		section {
			class: "flex min-h-0 flex-col gap-3 rounded-[10px] border p-3 {frame}",
			ondragover: move |e| {
				if accepts {
					e.prevent_default();
				}
			},
			ondrop: move |e| {
				e.prevent_default();
				let Some((review, from)) = dragging.take() else { return };
				match drop_means(from, column) {
					Some(true) => act(move |c| async move { c.reinstate(gmail, review).await.map(drop) }),
					Some(false) => act(move |c| async move { c.withdraw_reinstatement(gmail, review).await }),
					None => {}
				}
			},
			div { class: "flex items-center justify-between px-1",
				span { class: "text-[11px] font-medium uppercase tracking-wide text-ink-soft", "{title}" }
				span { class: "text-[11px] text-ink-soft", "{cards.len()}" }
			}
			div { class: "flex flex-col gap-3 overflow-y-auto",
				for card in cards {
					Card { key: "{card.review.id}", card: card.clone(), column, dragging }
				}
			}
		}
	}
}

#[component]
fn Card(card: BoardCard, column: Column, mut dragging: Signal<Option<(i64, Column)>>) -> Element {
	let r = &card.review;
	let movable = column != Column::Snapshotted;
	let stars = "★".repeat(r.rating.unwrap_or(0).clamp(0, 5) as usize);
	let when = match column {
		Column::Snapshotted => r.published_raw.clone().unwrap_or_else(|| format!("seen {}", ago(&r.first_seen))),
		Column::Removed => format!("gone {}", r.gone_at.as_deref().map(ago).unwrap_or_default()),
		Column::Reinstating => format!("requested {}", card.reinstatement.as_ref().map(|x| ago(&x.requested_at)).unwrap_or_default()),
	};
	let reinstated = card
		.reinstatement
		.as_ref()
		.filter(|_| column == Column::Snapshotted)
		.and_then(|x| Some(days_between(&x.requested_at, x.reinstated_at.as_deref()?)));
	let id = r.id;
	rsx! {
		article {
			class: if movable { "rounded-lg border border-border bg-card p-3 cursor-grab" } else { "rounded-lg border border-border bg-card p-3" },
			draggable: movable,
			ondragstart: move |e| {
				// Firefox starts no drag without data on it
				if let Some(dt) = e.as_web_event().data_transfer() {
					dt.set_data("text/plain", &id.to_string()).expect("setting drag data during dragstart");
				}
				dragging.set(Some((id, column)));
			},
			ondragend: move |_| dragging.set(None),
			if let Some(url) = r.capture_url.clone() {
				img { class: "w-full rounded-md bg-muted", src: "{url}" }
			}
			div { class: "mt-2 flex items-center gap-2",
				span { class: "text-accent-warn", "{stars}" }
				span { class: "truncate font-medium", "{r.author}" }
			}
			if let Some(text) = &r.text {
				p { class: "mt-1 line-clamp-3 text-ink-soft", "{text}" }
			}
			div { class: "mt-2 flex items-center justify-between text-[11px] text-ink-soft",
				span { "{when}" }
				if let Some(days) = reinstated {
					Badge { tone: Tone::Ok, "reinstated after {days}d" }
				}
			}
		}
	}
}

fn days_between(from: &str, to: &str) -> i64 {
	match (from.parse::<jiff::Timestamp>(), to.parse::<jiff::Timestamp>()) {
		(Ok(a), Ok(b)) => (b.as_second() - a.as_second()) / 86_400,
		_ => unreachable!("the archive writes timestamps in one shape"),
	}
}
