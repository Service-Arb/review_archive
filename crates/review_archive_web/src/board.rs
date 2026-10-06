//! One place's reviews in three tiles of a dock. A card is dragged from Removed to Reinstating when
//! its reinstatement was asked of Google, and back when that is withdrawn; the columns keep
//! their own order, and a review a scan lists again goes back to Snapshotted by itself.

use dioxus::{prelude::*, web::WebEventExt};
use dockviewers_dioxus::{Config, DockPanel, Group, GroupId, MinSize, PackedApi, PackedArea, PanelId, Step};
use review_archive_client::dto::{BoardCard, GmailDto, LocationSummary};

use crate::{Api, Badge, Refresh, Route, Tone, View, act, ago, shown};

#[derive(Clone, Copy, PartialEq)]
enum Column {
	Snapshotted,
	Removed,
	Reinstating,
}

const LANES: [(Column, &str, &str); 3] = [
	(Column::Snapshotted, "snapshotted", "Snapshotted"),
	(Column::Removed, "removed", "Removed"),
	(Column::Reinstating, "reinstating", "Reinstating"),
];

/// Maps the dock's chrome onto the kit's tokens.
const DOCK_THEME: &str = "--dv-group-bg: var(--secondary); --dv-tabstrip-bg: var(--secondary); --dv-tab-bg: var(--secondary); \
	--dv-tab-active-bg: var(--background); --dv-tab-active-fg: var(--ink); --dv-tab-border: var(--border); --dv-fg: var(--ink); \
	--dv-accent: var(--primary); --dv-shadow-bg: var(--hover); --dv-resize-bg: var(--border); --dv-content-pad: 0;";

#[component]
pub fn Board(gmail: GmailDto, location: LocationSummary, tab: Option<i64>) -> Element {
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
	let mut panels = use_signal(Vec::<DockPanel>::new);
	use_effect(move || {
		let Some(Ok(b)) = &*board.read() else { return };
		let mut lists = [b.snapshotted.clone(), b.removed.clone(), b.reinstating.clone()].into_iter();
		panels.set(
			LANES
				.iter()
				.map(|&(column, id, title)| {
					let cards = lists.next().expect("a list per lane");
					DockPanel {
						id: PanelId(id.into()),
						title: format!("{title} · {}", cards.len()),
						content: rsx! { Lane { column, cards, gmail: g, dragging } },
					}
				})
				.collect(),
		);
	});
	let mut dock = use_signal(|| None::<PackedApi>);
	// the tile `+`: brings back a lane closed with `✕`
	use_context_provider(|| {
		Callback::new(move |group: GroupId| {
			let mut api = dock().expect("tiles exist only after on_band");
			let open = api.tab_ids();
			if let Some(id) = LANES.iter().map(|l| PanelId(l.1.into())).find(|p| !open.contains(p)) {
				api.add_tab(group, id);
			}
		})
	});
	let config = Config {
		storage_key: Some("review-archive-board".into()),
		..Default::default()
	};
	let rows = config.rows;
	let on_band = Callback::new(move |mut api: PackedApi| {
		dock.set(Some(api));
		if api.restored() {
			return;
		}
		let w = api.cols() / 3;
		for (i, (_, id, _)) in LANES.iter().enumerate() {
			let group = Group::new(api.mint_group_id(), PanelId((*id).into()));
			let w = if i == LANES.len() - 1 { api.cols() - 2 * w } else { w };
			api.place(group, w, rows, MinSize::Steps { w: Step(2), h: Step(4) });
		}
	});
	rsx! {
		header { class: "flex h-14 shrink-0 items-center gap-3 border-b border-border px-6",
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
		if let Some(Err(e)) = &*board.read() {
			div { class: "border-b border-border px-6 py-2 text-accent-error", "{e}" }
		}
		div { class: "relative min-h-0 flex-1",
			div { class: "absolute inset-0", style: DOCK_THEME,
				PackedArea { panels, on_band: Some(on_band), config: Some(config) }
			}
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
fn Lane(column: Column, cards: Vec<BoardCard>, gmail: i64, dragging: Signal<Option<(i64, Column)>>) -> Element {
	let accepts = dragging().is_some_and(|(_, from)| drop_means(from, column).is_some());
	rsx! {
		section {
			class: if accepts { "flex h-full flex-col gap-3 overflow-y-auto p-3 font-sans leading-normal outline-2 -outline-offset-2 outline-dashed outline-primary-ink bg-primary/5" } else { "flex h-full flex-col gap-3 overflow-y-auto p-3 font-sans leading-normal" },
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
			for card in cards {
				Card { key: "{card.review.id}", card: card.clone(), column, dragging }
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
				div { class: "flex gap-1.5",
					if let Some(days) = reinstated {
						Badge { tone: Tone::Ok, "reinstated after {days}d" }
					}
					if r.reply.is_some() {
						Badge { tone: Tone::Ok, "responded" }
					} else {
						Badge { tone: Tone::Warn, "no reply" }
					}
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
