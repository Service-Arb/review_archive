//! The dashboard: a member's managing gmails, the places each tracks, and per place a board
//! of its reviews — snapshotted, removed, reinstating. Served by the archive under `/mfe/`,
//! so the API is the bundle's own origin, and the browser's `va_access` cookie signs its calls.
//! An admin gets tabs: their own dashboard, and any member's, acting as them.

#![cfg(target_arch = "wasm32")]
#![allow(clippy::useless_format)] // rsx! lowers every "{x}" to a format!

mod board;
mod telegram;

use dioxus::prelude::*;
use ev_lib::{
	i18n::Messages,
	mfe::bundle_origin,
	uikit::{
		self, BadgeVariant, Button, ButtonVariant, Card, CommandDialog, CommandEmpty, CommandInput, CommandItem, CommandList, InfoTip, InfoTipContent, InfoTipTrigger, Input, Size, Tabs,
		TabsList, TabsTrigger,
	},
};
use review_archive_client::{
	Client,
	dto::{GmailOverview, LocationSummary, MemberDto, NewTrack, RunStatus},
};

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

/// The API as this tab's member: the caller, or the member an admin's tab acts as.
#[derive(Clone)]
struct Api(Client);

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

fn api() -> Client {
	Client::ambient(reqwest::Client::new(), &bundle_origin()).expect("the bundle's origin is a URL")
}

/// A failed call, as shown. A 401 means the sign-in cookie is gone or expired: the whole
/// page goes to the element's `sign-in` URL, which sends the browser back here signed in.
fn shown(e: review_archive_client::Error) -> String {
	if e.status().map(|s| s.as_u16()) == Some(401) {
		let window = web_sys::window().expect("a browser");
		let sign_in = window
			.document()
			.expect("a page")
			.query_selector(TAG)
			.expect("a valid selector")
			.expect("mounted inside its element")
			.get_attribute("sign-in")
			.expect("the host page names where to sign in");
		let here = window.location().href().expect("a page has a URL");
		let to = format!("{sign_in}?return_to={}", String::from(js_sys::encode_uri_component(&here)));
		window
			.top()
			.expect("a browsing context")
			.expect("a top window")
			.location()
			.set_href(&to)
			.expect("navigating the top window");
	}
	e.to_string()
}

/// Runs a write against the API, then reads everything again; a failure is shown.
fn act<F: Future<Output = Result<(), review_archive_client::Error>> + 'static>(f: impl FnOnce(Client) -> F + 'static) {
	let Api(api) = consume_context();
	let Refresh(mut refresh) = consume_context();
	let Failure(mut failure) = consume_context();
	spawn(async move {
		match f(api).await {
			Ok(()) => failure.set(None),
			Err(e) => failure.set(Some(shown(e))),
		}
		refresh += 1;
	});
}

const SHELL: &str = "flex min-h-[640px] bg-background text-ink text-[13px] font-sans";

#[component]
fn Dashboard() -> Element {
	let me = use_resource(|| async { api().me().await.map_err(shown) });
	match &*me.read() {
		Some(Ok(me)) if me.admin => rsx! { Admin { email: me.email.clone() } },
		Some(Ok(_)) => rsx! { Workspace { member: None } },
		Some(Err(e)) => rsx! { div { class: "{SHELL} p-6 text-accent-error", "{e}" } },
		None => rsx! { div { class: "{SHELL} p-6 text-ink-soft", "Loading…" } },
	}
}

/// "You", then a tab per member opened; every tab stays mounted, so switching keeps where
/// each one was.
#[component]
fn Admin(email: String) -> Element {
	const YOU: &str = "";
	let mut tabs = use_signal(Vec::<MemberDto>::new);
	let mut active = use_signal(|| YOU.to_owned());
	let mut picking = use_signal(|| false);
	let shown_if = |value: &str| if active() == value { "" } else { "hidden" };
	rsx! {
		div { class: "flex flex-col bg-background",
			Tabs { value: active(), on_value_change: move |v| active.set(v), class: "border-b border-border bg-secondary px-2 py-1.5",
				div { class: "flex items-center gap-1",
					TabsList { class: "bg-transparent",
						TabsTrigger { value: YOU, "You" }
						for m in tabs() {
							div { key: "{m.email}", class: "flex items-center",
								TabsTrigger { value: m.email.clone(), {m.username.clone().unwrap_or_else(|| m.email.clone())} }
								Button {
									variant: ButtonVariant::Ghost,
									size: Size::Xs,
									icon: true,
									r#type: "button",
									onclick: move |_| {
										tabs.write().retain(|t| t.email != m.email);
										if active() == m.email {
											active.set(YOU.to_owned());
										}
									},
									"×"
								}
							}
						}
					}
					Button {
						variant: ButtonVariant::Ghost,
						size: Size::Xs,
						icon: true,
						r#type: "button",
						onclick: move |_| picking.set(true),
						"+"
					}
				}
			}
			div { class: shown_if(YOU), Workspace { member: None } }
			for m in tabs() {
				div { key: "{m.email}", class: shown_if(&m.email), Workspace { member: Some(m.email.clone()) } }
			}
			if picking() {
				Picker {
					on_pick: move |m: MemberDto| {
						picking.set(false);
						if m.email == email {
							active.set(YOU.to_owned());
							return;
						}
						if !tabs.read().iter().any(|t| t.email == m.email) {
							tabs.write().push(m.clone());
						}
						active.set(m.email);
					},
					on_close: move |_| picking.set(false),
				}
			}
		}
	}
}

/// fzf over the members valeratrades.com lists: name and email.
#[component]
fn Picker(on_pick: EventHandler<MemberDto>, on_close: EventHandler<()>) -> Element {
	let members = use_resource(|| async { api().members().await.map_err(shown) });
	rsx! {
		CommandDialog {
			open: true,
			on_open_change: move |open: bool| {
				if !open {
					on_close.call(());
				}
			},
			CommandInput { placeholder: "Member's name or email" }
			CommandList {
				match &*members.read() {
					Some(Ok(members)) => rsx! {
						for m in members.clone() {
							CommandItem {
								key: "{m.email}",
								value: format!("{} {}", m.username.as_deref().unwrap_or_default(), m.email),
								on_select: {
									let m = m.clone();
									move |_| on_pick.call(m.clone())
								},
								match &m.username {
									Some(name) => rsx! { span { "{name}" } },
									None => rsx! { uikit::Badge { variant: BadgeVariant::Outline, "not signed up" } },
								}
								span { class: "truncate text-ink-soft", "{m.email}" }
							}
						}
						CommandEmpty { "No member matches." }
					},
					Some(Err(e)) => rsx! { div { class: "p-3 text-accent-error", "{e}" } },
					None => rsx! { div { class: "p-3 text-ink-soft", "Loading…" } },
				}
			}
		}
	}
}

/// One member's dashboard: the signed-in person's own, or (`member`) the one an admin acts as.
#[component]
fn Workspace(member: Option<String>) -> Element {
	let Api(client) = use_context_provider(|| {
		Api(match &member {
			Some(m) => api().as_member(m.clone()),
			None => api(),
		})
	});
	let refresh = use_context_provider(|| Refresh(Signal::new(0)));
	let failure = use_context_provider(|| Failure(Signal::new(None)));
	let mut page = use_signal(|| Page::Places);
	let mut gmail = use_signal(|| None::<i64>);
	let overview = use_resource(move || {
		let client = client.clone();
		async move {
			refresh.0();
			client.overview().await.map_err(shown)
		}
	});

	let shell = SHELL;
	let gmails: Vec<GmailOverview> = match &*overview.read() {
		Some(Ok(g)) => g.clone(),
		Some(Err(e)) => return rsx! { div { class: "{shell} p-6 text-accent-error", "{e}" } },
		_ => return rsx! { div { class: "{shell} p-6 text-ink-soft", "Loading…" } },
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
				if let Some(m) = &member {
					div { class: "border-b border-border bg-accent-warn/15 px-6 py-2 text-accent-warn", "Viewing as {m} — actions apply to their account" }
				}
				if let Some(e) = failure.0() {
					div { class: "border-b border-border bg-accent-error/15 px-6 py-2 text-accent-error", "{e}" }
				}
				match (page(), scope) {
					(Page::Telegram, _) => rsx! {
						telegram::Channels { gmails: gmails.iter().map(|g| g.gmail.clone()).collect::<Vec<_>>() }
					},
					(_, None) => rsx! {
						div { class: "p-6 text-ink-soft", "Add the gmail your places are managed from, on the left." }
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
	let row = "flex items-center gap-2 rounded-md px-3 py-2 text-left cursor-pointer hover:bg-hover";
	rsx! {
		nav { class: "flex w-60 shrink-0 flex-col border-r border-border bg-secondary",
			div { class: "px-4 py-4 text-[14px] font-semibold", "review_archive" }
			div { class: "flex items-center gap-1.5 px-4 pb-1 text-[11px] font-medium uppercase tracking-wide text-ink-soft",
				"Managing gmails"
				InfoTip {
					InfoTipTrigger { label: "What a managing gmail is" }
					InfoTipContent { class: "normal-case font-normal tracking-normal flex flex-col gap-2",
						p { "Full gmail address or any alias for it. Used only to group places; it doesn't affect any actions taken." }
						p { "If no managing account is connected, enter the email the place is on, or its shorthand." }
					}
				}
			}
			div { class: "flex flex-col gap-0.5 px-2",
				for g in gmails {
					button {
						key: "{g.gmail.id}",
						class: if current == Some(g.gmail.id) && !telegram { "{row} bg-hover text-ink" } else { "{row} text-ink-soft" },
						onclick: move |_| on_pick.call(g.gmail.id),
						span { class: "truncate flex-1", "{g.gmail.gmail}" }
						uikit::Badge { variant: BadgeVariant::Secondary, "{g.locations.len()}" }
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
				Input {
					size: Size::Sm,
					placeholder: "+ Add gmail",
					value: adding(),
					oninput: move |e: FormEvent| adding.set(e.value()),
				}
			}
			div { class: "flex-1" }
			button {
				class: if telegram { "{row} mx-2 mb-3 bg-hover text-ink" } else { "{row} mx-2 mb-3 text-ink-soft" },
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
		header { class: "flex h-14 items-center gap-3 border-b border-border px-6",
			span { class: "text-ink-soft", "{scope.gmail.gmail}" }
			span { class: "text-ink-soft", "/" }
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
				Input {
					class: "w-80",
					placeholder: "Place id or Google Maps URL",
					value: place(),
					oninput: move |e: FormEvent| place.set(e.value()),
				}
				Button { "+ Track place" }
			}
		}
		main { class: "p-6",
			if scope.locations.is_empty() {
				div { class: "text-ink-soft", "No places tracked under this gmail yet." }
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
		Some(RunStatus::Ok) => ("bg-positive", "ok"),
		Some(RunStatus::Partial) => ("bg-accent-warn", "partial"),
		Some(RunStatus::Failed) => ("bg-accent-error", "failed"),
		None => ("bg-ink-soft", "never scanned"),
	};
	let last = loc.last_run_at.as_deref().map(|t| format!("last run {} · ", ago(t))).unwrap_or_default();
	rsx! {
		button { class: "text-left", onclick: move |_| on_open.call(()),
			Card { class: "gap-3 rounded-lg p-4 py-4 hover:border-ink-soft",
				div {
					div { class: "truncate text-[14px] font-semibold", "{loc.target.label}" }
					div { class: "truncate font-mono text-[11px] text-ink-soft", "{loc.target.place_id}" }
				}
				div { class: "flex items-baseline gap-3",
					span { class: "text-[28px] font-semibold leading-none", "{loc.snapshots_7d}" }
					span { class: "text-ink-soft", "snapshots 7d" }
					span { class: "text-[15px] text-ink-soft", "{loc.snapshots_30d}" }
					span { class: "text-ink-soft", "30d" }
				}
				div { class: "flex flex-wrap gap-1.5",
					Badge { tone: Tone::Ok, "live {loc.live}" }
					if loc.removed > 0 {
						Badge { tone: Tone::Bad, "removed {loc.removed}" }
					}
					if loc.reinstating > 0 {
						Badge { tone: Tone::Warn, "reinstating {loc.reinstating}" }
					}
				}
				div { class: "flex items-center gap-2 border-t border-border pt-3 text-[11px] text-ink-soft",
					span { class: "size-1.5 rounded-full {dot}" }
					"{last}{status}"
				}
			}
		}
	}
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
	Ok,
	Bad,
	Warn,
}

#[component]
fn Badge(tone: Tone, children: Element) -> Element {
	let (variant, class) = match tone {
		Tone::Ok => (BadgeVariant::Success, ""),
		Tone::Bad => (BadgeVariant::Outline, "border-transparent bg-accent-error/15 text-accent-error"),
		Tone::Warn => (BadgeVariant::Outline, "border-transparent bg-accent-warn/15 text-accent-warn"),
	};
	rsx! {
		uikit::Badge { variant, class, {children} }
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
