//! The dashboard: a member's managing gmails, the places each tracks, and per place a board
//! of its reviews — snapshotted, removed, reinstating. Served by the archive under `/mfe/`,
//! so the API is the bundle's own origin; the host hands the member's playbook token in as
//! the element's `access-token` attribute.

#![cfg(target_arch = "wasm32")]
#![allow(clippy::useless_format)] // rsx! lowers every "{x}" to a format!

mod board;
mod telegram;

use dioxus::prelude::*;
use ev_lib::{i18n::Messages, mfe::bundle_origin};
use futures::StreamExt;
use review_archive_client::{
	Client,
	dto::{GmailOverview, LocationSummary, NewTrack, RunStatus},
};
use wasm_bindgen::{JsCast, closure::Closure};

ev_lib::mfe! {
	service: "review-archive", name: "dashboard", kind: page,
	root: crate::Dashboard, stylesheet: "mfe.css",
	messages: crate::catalogue
}

/// English only, for now: the copy is written where it renders.
fn catalogue(_: ev_lib::i18n::Locale) -> Messages {
	Messages::new()
}

const TAG: &str = "mfe-review-archive-dashboard";

/// The member's token, as the host last set it.
#[derive(Clone, Copy)]
struct Token(Signal<Option<String>>);

/// Bumped after a write, so what was read is read again.
#[derive(Clone, Copy)]
struct Refresh(Signal<u32>);

/// The last action that failed, shown until the next one.
#[derive(Clone, Copy)]
struct Failure(Signal<Option<String>>);

#[derive(Clone, Copy, PartialEq)]
enum Page {
	Places,
	Board(i64),
	Telegram,
}

fn api(token: &str) -> Client {
	Client::new(&bundle_origin(), token).expect("the bundle's origin is a URL")
}

/// Runs a write against the API, then reads everything again; a failure is shown.
fn act<F: Future<Output = Result<(), review_archive_client::Error>> + 'static>(f: impl FnOnce(Client) -> F + 'static) {
	let Token(token) = consume_context();
	let Refresh(mut refresh) = consume_context();
	let Failure(mut failure) = consume_context();
	let Some(t) = token() else { return };
	spawn(async move {
		match f(api(&t)).await {
			Ok(()) => failure.set(None),
			Err(e) => failure.set(Some(e.to_string())),
		}
		refresh += 1;
	});
}

/// Follows the element's `access-token` attribute: a host may set it after mounting, and
/// sets it again when the token is refreshed.
fn use_access_token() -> Signal<Option<String>> {
	let mut token = use_signal(|| None);
	use_future(move || async move {
		let el = web_sys::window()
			.expect("a browser")
			.document()
			.expect("a page")
			.query_selector(TAG)
			.expect("a valid selector")
			.expect("mounted inside its element");
		let read = {
			let el = el.clone();
			move || el.get_attribute("access-token").filter(|t| !t.is_empty())
		};
		token.set(read());
		let (tx, mut rx) = futures::channel::mpsc::unbounded();
		let changed = Closure::<dyn FnMut()>::new(move || tx.unbounded_send(()).expect("the receiver lives as long as the app"));
		let observer = web_sys::MutationObserver::new(changed.as_ref().unchecked_ref()).expect("MutationObserver takes a callback");
		let init = web_sys::MutationObserverInit::new();
		init.set_attributes(true);
		init.set_attribute_filter(&js_sys::Array::of1(&"access-token".into()));
		observer.observe_with_options(&el, &init).expect("observing an element's attribute");
		// the app lives as long as the page (see `ev_lib::mfe`), and so does its observer
		changed.forget();
		while rx.next().await.is_some() {
			token.set(read());
		}
	});
	token
}

#[component]
fn Dashboard() -> Element {
	let token = use_access_token();
	use_context_provider(|| Token(token));
	let refresh = use_context_provider(|| Refresh(Signal::new(0)));
	let failure = use_context_provider(|| Failure(Signal::new(None)));
	let mut page = use_signal(|| Page::Places);
	let mut gmail = use_signal(|| None::<i64>);
	let overview = use_resource(move || async move {
		refresh.0();
		let t = token()?;
		Some(api(&t).overview().await.map_err(|e| e.to_string()))
	});

	let shell = "flex min-h-[640px] bg-page text-fg text-[13px] font-sans";
	if token().is_none() {
		return rsx! {
			div { class: "{shell} items-center justify-center text-muted", "Sign in to see your places: the page hosting this dashboard gives it your access token." }
		};
	};
	let gmails: Vec<GmailOverview> = match &*overview.read() {
		Some(Some(Ok(g))) => g.clone(),
		Some(Some(Err(e))) => return rsx! { div { class: "{shell} p-6 text-bad", "{e}" } },
		_ => return rsx! { div { class: "{shell} p-6 text-muted", "Loading…" } },
	};
	// the first gmail until one is picked; a removed one falls back the same way
	let current = gmail().filter(|id| gmails.iter().any(|g| g.gmail.id == *id)).or_else(|| gmails.first().map(|g| g.gmail.id));
	let scope = current.and_then(|id| gmails.iter().find(|g| g.gmail.id == id)).cloned();

	rsx! {
		div { class: "{shell}",
			Rail {
				gmails: gmails.clone(),
				current,
				telegram: page() == Page::Telegram,
				on_pick: move |id| {
					gmail.set(Some(id));
					page.set(Page::Places);
				},
				on_telegram: move |_| page.set(Page::Telegram),
			}
			div { class: "flex flex-1 flex-col min-w-0",
				if let Some(e) = failure.0() {
					div { class: "border-b border-line bg-bad/15 px-6 py-2 text-bad", "{e}" }
				}
				match (page(), scope) {
					(Page::Telegram, _) => rsx! {
						telegram::Channels { gmails: gmails.iter().map(|g| g.gmail.clone()).collect::<Vec<_>>() }
					},
					(_, None) => rsx! {
						div { class: "p-6 text-muted", "Add the gmail your places are managed from, on the left." }
					},
					(Page::Board(target), Some(g)) => match g.locations.iter().find(|l| l.target.id == target) {
						Some(loc) => rsx! {
							board::Board {
								gmail: g.gmail.clone(),
								location: loc.clone(),
								on_back: move |_| page.set(Page::Places),
							}
						},
						None => rsx! { Places { scope: g, on_open: move |t| page.set(Page::Board(t)) } },
					},
					(Page::Places, Some(g)) => rsx! { Places { scope: g, on_open: move |t| page.set(Page::Board(t)) } },
				}
			}
		}
	}
}

#[component]
fn Rail(gmails: Vec<GmailOverview>, current: Option<i64>, telegram: bool, on_pick: EventHandler<i64>, on_telegram: EventHandler<()>) -> Element {
	let mut adding = use_signal(String::new);
	let row = "flex items-center gap-2 rounded-md px-3 py-2 text-left cursor-pointer hover:bg-raised";
	rsx! {
		nav { class: "flex w-60 shrink-0 flex-col border-r border-line bg-panel",
			div { class: "px-4 py-4 text-[14px] font-semibold", "review_archive" }
			div { class: "px-4 pb-1 text-[11px] font-medium uppercase tracking-wide text-faint", "Managing gmails" }
			div { class: "flex flex-col gap-0.5 px-2",
				for g in gmails {
					button {
						key: "{g.gmail.id}",
						class: if current == Some(g.gmail.id) && !telegram { "{row} bg-raised text-fg" } else { "{row} text-muted" },
						onclick: move |_| on_pick.call(g.gmail.id),
						span { class: "truncate flex-1", "{g.gmail.gmail}" }
						span { class: "text-[11px] text-faint", "{g.locations.len()}" }
					}
				}
			}
			form {
				class: "mt-2 flex gap-1 px-3",
				onsubmit: move |e| {
					e.prevent_default();
					let gmail = adding();
					act(move |c| async move { c.add_gmail(&gmail).await.map(drop) });
					adding.set(String::new());
				},
				input {
					class: "min-w-0 flex-1 rounded-md border border-line bg-page px-2 py-1.5 text-fg placeholder:text-faint focus:border-accent focus:outline-none",
					placeholder: "+ Add gmail",
					value: "{adding}",
					oninput: move |e| adding.set(e.value()),
				}
			}
			div { class: "flex-1" }
			button {
				class: if telegram { "{row} mx-2 mb-3 bg-raised text-fg" } else { "{row} mx-2 mb-3 text-muted" },
				onclick: move |_| on_telegram.call(()),
				"Telegram alerts"
			}
		}
	}
}

/// The gmail's places as cards, most screenshots over the last week first.
#[component]
fn Places(scope: GmailOverview, on_open: EventHandler<i64>) -> Element {
	let mut place = use_signal(String::new);
	let gmail = scope.gmail.id;
	rsx! {
		header { class: "flex h-14 items-center gap-3 border-b border-line px-6",
			span { class: "text-muted", "{scope.gmail.gmail}" }
			span { class: "text-faint", "/" }
			span { class: "font-medium", "Locations" }
			div { class: "flex-1" }
			form {
				class: "flex gap-2",
				onsubmit: move |e| {
					e.prevent_default();
					let req = NewTrack { place: place(), ..Default::default() };
					act(move |c| async move { c.track(gmail, &req).await.map(drop) });
					place.set(String::new());
				},
				input {
					class: "w-80 rounded-md border border-line bg-page px-2.5 py-2 text-fg placeholder:text-faint focus:border-accent focus:outline-none",
					placeholder: "Place id or Google Maps URL",
					value: "{place}",
					oninput: move |e| place.set(e.value()),
				}
				button { class: "rounded-md bg-accent px-3 py-1.5 font-medium text-on-accent", "+ Track place" }
			}
		}
		main { class: "p-6",
			if scope.locations.is_empty() {
				div { class: "text-muted", "No places tracked under this gmail yet." }
			}
			div { class: "grid grid-cols-[repeat(auto-fill,minmax(320px,1fr))] gap-4",
				for loc in scope.locations {
					LocationCard { key: "{loc.target.id}", loc: loc.clone(), on_open: move |_| on_open.call(loc.target.id) }
				}
			}
		}
	}
}

#[component]
fn LocationCard(loc: LocationSummary, on_open: EventHandler<()>) -> Element {
	let (dot, status) = match loc.last_run_status {
		Some(RunStatus::Ok) => ("bg-ok", "ok"),
		Some(RunStatus::Partial) => ("bg-warn", "partial"),
		Some(RunStatus::Failed) => ("bg-bad", "failed"),
		None => ("bg-faint", "never scanned"),
	};
	let last = loc.last_run_at.as_deref().map(|t| format!("last run {} · ", ago(t))).unwrap_or_default();
	rsx! {
		button {
			class: "flex flex-col gap-3 rounded-lg border border-line bg-panel p-4 text-left hover:border-faint",
			onclick: move |_| on_open.call(()),
			div {
				div { class: "truncate text-[14px] font-semibold", "{loc.target.label}" }
				div { class: "truncate font-mono text-[11px] text-faint", "{loc.target.place_id}" }
			}
			div { class: "flex items-baseline gap-3",
				span { class: "text-[28px] font-semibold leading-none", "{loc.snapshots_7d}" }
				span { class: "text-muted", "snapshots 7d" }
				span { class: "text-[15px] text-muted", "{loc.snapshots_30d}" }
				span { class: "text-faint", "30d" }
			}
			div { class: "flex flex-wrap gap-1.5",
				Badge { tone: "ok", "live {loc.live}" }
				if loc.removed > 0 {
					Badge { tone: "bad", "removed {loc.removed}" }
				}
				if loc.reinstating > 0 {
					Badge { tone: "warn", "reinstating {loc.reinstating}" }
				}
			}
			div { class: "flex items-center gap-2 border-t border-line pt-3 text-[11px] text-muted",
				span { class: "size-1.5 rounded-full {dot}" }
				"{last}{status}"
			}
		}
	}
}

#[component]
fn Badge(tone: &'static str, children: Element) -> Element {
	let class = match tone {
		"ok" => "bg-ok/15 text-ok",
		"bad" => "bg-bad/15 text-bad",
		"warn" => "bg-warn/15 text-warn",
		_ => "bg-raised text-muted",
	};
	rsx! {
		span { class: "rounded px-2 py-0.5 text-[12px] {class}", {children} }
	}
}

/// "3h ago" for a stored timestamp.
fn ago(ts: &str) -> String {
	let Ok(t) = ts.parse::<jiff::Timestamp>() else { return ts.to_owned() };
	let secs = (js_sys::Date::now() as i64 / 1000 - t.as_second()).max(0);
	match secs {
		s if s < 90 => "just now".into(),
		s if s < 90 * 60 => format!("{}m ago", s / 60),
		s if s < 36 * 3600 => format!("{}h ago", s / 3600),
		s => format!("{}d ago", s / 86_400),
	}
}
