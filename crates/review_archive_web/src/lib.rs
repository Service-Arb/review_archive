//! The dashboard: a person's managing gmails, the places each tracks, and per place a board
//! of its reviews — snapshotted, removed, reinstating. The Service-Arb panel mounts it and
//! forwards its calls to the archive: the panel's session cookie and CSRF header sign them.
//! One who may act as others picks a member at `/act-as`, from the panel's account menu.

#![cfg(target_arch = "wasm32")]
#![allow(clippy::useless_format)] // rsx! lowers every "{x}" to a format!

mod board;
mod telegram;
mod tokens;

use std::sync::OnceLock;

use dioxus::prelude::*;
use ev_lib::{
	i18n::{Locale, Messages, Translator},
	t,
	uikit::{
		self, BadgeVariant, Button, ButtonVariant, Card, CommandDialog, CommandEmpty, CommandInput, CommandItem, CommandList, InfoTip, InfoTipContent, InfoTipTrigger, Input, Size, Switch,
	},
};
use review_archive_client::{
	Client,
	dto::{BalanceChange, GmailOverview, LocationSummary, Me, MemberDto, NewTrack, RunStatus, TokensChange, TokensDto},
};
use sa_auth::{Members, Permission, PermissionSet, Tokens};
use wasm_bindgen::{JsCast, prelude::*};

const TAG: &str = "mfe-review-archive-dashboard";

/// English only, for now: each key's English is written where it renders.
fn catalogue(_: Locale) -> Messages {
	Messages::new()
}

/// The element's attributes, all required.
struct Host {
	/// The API's base URL, resolved against the page.
	api: String,
	/// Where a browser signs in; it comes back to the same-origin path in `return_to`.
	sign_in: String,
	/// The cookie whose value every request carries as `x-sa-csrf`.
	csrf_cookie: String,
	/// The path the page is served at; the element's views are paths under it.
	base: String,
}

/// The entry module's URL: every file of the bundle sits next to it.
static BUNDLE: OnceLock<String> = OnceLock::new();
/// The host page's language, and its attributes or the names of those it left out.
static MOUNT: OnceLock<(Locale, Result<Host, Vec<&'static str>>)> = OnceLock::new();

/// Called by the entry module with its own URL; registers the element.
#[wasm_bindgen]
pub fn define(bundle: String) {
	BUNDLE.set(bundle).expect("the entry module runs once");
	let mount = Closure::<dyn Fn(web_sys::Element)>::new(mount);
	ev_lib::mfe::register(TAG, &mount);
	mount.forget(); // the element can mount at any time after registration
}

fn mount(el: web_sys::Element) {
	let attr = |name| el.get_attribute(name);
	let host = match (attr("api"), attr("sign-in"), attr("csrf-cookie"), attr("base")) {
		(Some(api), Some(sign_in), Some(csrf_cookie), Some(base)) => Ok(Host { api, sign_in, csrf_cookie, base }),
		_ => Err(["api", "sign-in", "csrf-cookie", "base"].into_iter().filter(|a| !el.has_attribute(a)).collect()),
	};
	let mut cfg = dioxus::web::Config::new().rootelement(el.clone());
	if let Ok(host) = &host {
		cfg = cfg.history(std::rc::Rc::new(dioxus::web::WebHistory::new(Some(host.base.clone()), true)));
	}
	assert!(MOUNT.set((ev_lib::mfe::host_locale(&el), host)).is_ok(), "one dashboard per page");
	dioxus::LaunchBuilder::new().with_cfg(cfg).launch(root);
}

fn root() -> Element {
	let (locale, host) = MOUNT.get().expect("set before launch");
	let tr = use_context_provider(|| Translator::new(catalogue(*locale), *locale));
	let bundle = BUNDLE.get().expect("defined before any mount");
	let stylesheet = reqwest::Url::parse(bundle).and_then(|b| b.join("mfe.css")).expect("the entry module's URL is a URL");
	rsx! {
		document::Stylesheet { href: stylesheet.to_string() }
		match host {
			Ok(_) => rsx! { Router::<Route> {} },
			Err(missing) => rsx! {
				div { class: "p-6 text-accent-error text-[13px] font-sans",
					{t!(tr, "host.missing", "<{tag}> is missing its attributes: {missing}", tag = TAG, missing = missing.join(", "))}
				}
			},
		}
	}
}

fn host() -> &'static Host {
	MOUNT
		.get()
		.and_then(|(_, h)| h.as_ref().ok())
		.expect("only the router asks, and it renders once the attributes are read")
}

/// The API as this tab's member: the caller, or the member their tab acts as.
#[derive(Clone)]
struct Api(Client);

/// Bumped after a write, so what was read is read again.
#[derive(Clone, Copy)]
struct Refresh(Signal<u32>);

/// The last action that failed, shown until the next one.
#[derive(Clone, Copy)]
struct Failure(Signal<Option<String>>);

/// What the signed-in person may do here.
#[derive(Clone)]
struct Held(PermissionSet);

fn may(p: impl Permission) -> bool {
	let Held(held) = consume_context();
	held.may(p)
}

/// What a tab shows.
#[derive(Clone, Debug, PartialEq)]
enum View {
	Home,
	Gmail(i64),
	Place { gmail: i64, target: i64 },
	Telegram,
	Tokens,
}

impl dioxus::router::ToRouteSegments for View {
	fn display_route_segments(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Home => write!(f, "/"), // an empty path would be no URL at all: a link to it goes nowhere
			Self::Gmail(g) => write!(f, "/gmails/{g}"),
			Self::Place { gmail, target } => write!(f, "/gmails/{gmail}/places/{target}"),
			Self::Telegram => write!(f, "/telegram"),
			Self::Tokens => write!(f, "/tokens"),
		}
	}
}

impl dioxus::router::FromRouteSegments for View {
	type Err = String;

	fn from_route_segments(segments: &[&str]) -> Result<Self, String> {
		let id = |s: &str| s.parse::<i64>().map_err(|_| format!("{s:?} is not an id"));
		match segments {
			[] | [""] => Ok(Self::Home),
			["gmails", g] => Ok(Self::Gmail(id(g)?)),
			["gmails", g, "places", t] => Ok(Self::Place { gmail: id(g)?, target: id(t)? }),
			["telegram"] => Ok(Self::Telegram),
			["tokens"] => Ok(Self::Tokens),
			_ => Err(format!("no view at /{}", segments.join("/"))),
		}
	}
}

/// Where the dashboard is, as its URL under the host's `base` says: a view of the caller's
/// own, of the member they act as, or the pick of one.
#[derive(Clone, Debug, PartialEq, Routable)]
#[rustfmt::skip]
enum Route {
	#[layout(Shell)]
		#[route("/act-as", Pick)]
		Pick {},
		#[route("/members/:member/:..view", TheirView)]
		Theirs { member: i64, view: View },
		#[route("/:..view", MyView)]
		Mine { view: View },
}

impl Route {
	fn at(member: Option<i64>, view: View) -> Self {
		match member {
			Some(member) => Self::Theirs { member, view },
			None => Self::Mine { view },
		}
	}
}

#[component]
fn MyView(view: View) -> Element {
	rsx! { Workspace { tab: None, at: view } }
}

#[component]
fn TheirView(member: i64, view: View) -> Element {
	rsx! { Workspace { key: "{member}", tab: member, at: view } } // a workspace's `Api` is fixed at mount
}

fn api() -> Client {
	let Host { api, csrf_cookie, .. } = host();
	let window = web_sys::window().expect("a browser");
	let page = window.location().href().expect("a page has a URL");
	let base = reqwest::Url::parse(&page).and_then(|p| p.join(api)).expect("`api` resolves against the page");
	let cookies = window
		.document()
		.expect("a page")
		.dyn_into::<web_sys::HtmlDocument>()
		.expect("an HTML page")
		.cookie()
		.expect("the page's cookies are readable");
	let mut headers = reqwest::header::HeaderMap::new();
	// absent, the panel refuses a write with its own answer
	if let Some(token) = cookies.split("; ").find_map(|c| c.strip_prefix(csrf_cookie.as_str())?.strip_prefix('=')) {
		headers.insert("x-sa-csrf", reqwest::header::HeaderValue::from_str(token).expect("a cookie value is a valid header value"));
	}
	let http = reqwest::Client::builder()
		.default_headers(headers)
		.build()
		.expect("a browser client takes no setup that can fail");
	Client::ambient(http, base.as_str()).expect("resolved to an absolute URL")
}

/// A failed call, as shown. A 401 means the panel's session is gone: the whole page goes to
/// the element's `sign-in`, which brings the browser back here signed in.
fn shown(e: review_archive_client::Error) -> String {
	if e.status().map(|s| s.as_u16()) == Some(401) {
		let location = web_sys::window().expect("a browser").location();
		let here = format!(
			"{}{}{}",
			location.pathname().expect("a page has a path"),
			location.search().expect("a page has a query"),
			location.hash().expect("a page has a fragment")
		);
		let to = format!("{}?return_to={}", host().sign_in, String::from(js_sys::encode_uri_component(&here)));
		location.set_href(&to).expect("navigating the page");
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

const SHELL: &str = "flex min-h-0 flex-1 bg-background text-ink text-[13px] font-sans";

#[component]
fn Shell() -> Element {
	let me = use_resource(|| async { api().me().await.map_err(shown) });
	match &*me.read() {
		Some(Ok(me)) => rsx! { Signed { me: me.clone() } },
		Some(Err(e)) => rsx! { div { class: "{SHELL} p-6 text-accent-error", "{e}" } },
		None => rsx! { div { class: "{SHELL} p-6 text-ink-soft", "Loading…" } },
	}
}

#[component]
fn Signed(me: Me) -> Element {
	use_context_provider(|| Held(me.permissions.iter().cloned().collect()));
	use_context_provider(|| me.clone());
	rsx! {
		div { class: "flex h-full flex-col", Outlet::<Route> {} }
	}
}

/// The picker over one's own dashboard; picking a member acts as them, unless one only grants
/// tokens.
#[component]
fn Pick() -> Element {
	let me: Me = use_context();
	let acts = may(Members::ActAs);
	rsx! {
		Workspace { tab: None, at: View::Home }
		Picker {
			on_pick: move |m: MemberDto| {
				if acts {
					navigator().replace(Route::at((m.id != me.id).then_some(m.id), View::Home));
				}
			},
			on_close: move |_| {
				navigator().replace(Route::Mine { view: View::Home });
			},
		}
	}
}

/// fzf over everyone who signed in here: name and email, and their balance.
#[component]
fn Picker(on_pick: EventHandler<MemberDto>, on_close: EventHandler<()>) -> Element {
	let tr: Translator = use_context();
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
								key: "{m.id}",
								value: format!("{} {}", m.name, m.email),
								on_select: {
									let m = m.clone();
									move |_| on_pick.call(m.clone())
								},
								if !m.claimed {
									uikit::Badge { variant: BadgeVariant::Outline, {t!(tr, "picker.unclaimed", "never signed in")} }
								}
								span { "{m.name}" }
								span { class: "flex-1 truncate text-ink-soft", "{m.email}" }
								Balance { member: m.id, balance: m.balance }
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

/// A member's balance in the picker, and a change of it.
#[component]
fn Balance(member: i64, balance: i64) -> Element {
	let mut balance = use_signal(|| balance);
	let mut amount = use_signal(String::new);
	let mut failure = use_signal(|| None::<String>);
	let mut change = move |to: fn(i64) -> BalanceChange| {
		let n = match amount().trim().parse::<i64>() {
			Ok(n) => n,
			Err(e) => return failure.set(Some(format!("{:?}: {e}", amount()))),
		};
		spawn(async move {
			match api().change_tokens(member, &TokensChange { change: to(n), note: None }).await {
				Ok(t) => {
					balance.set(t.balance);
					amount.set(String::new());
					failure.set(None);
				}
				Err(e) => failure.set(Some(shown(e))),
			}
		});
	};
	let mut set = change;
	rsx! {
		// the row's click picks the member: these are not that
		div { class: "flex shrink-0 items-center gap-1", onclick: |e| e.stop_propagation(),
			if let Some(e) = failure() {
				span { class: "max-w-40 truncate text-[11px] text-accent-error", title: "{e}", "{e}" }
			}
			span { class: "w-20 text-right font-mono text-ink-soft", "{balance} tokens" }
			Input {
				class: "w-16",
				size: Size::Xs,
				value: amount(),
				oninput: move |e: FormEvent| amount.set(e.value()),
			}
			Button { variant: ButtonVariant::Outline, size: Size::Xs, r#type: "button", onclick: move |_| set(BalanceChange::Set), "set" }
			Button { variant: ButtonVariant::Outline, size: Size::Xs, r#type: "button", onclick: move |_| change(BalanceChange::Grant), "grant" }
		}
	}
}

/// The dashboard at `at`: the signed-in person's own, or the one `tab` they act as.
#[component]
fn Workspace(tab: Option<i64>, at: View) -> Element {
	let tr: Translator = use_context();
	let Api(client) = use_context_provider(|| {
		Api(match tab {
			Some(m) => api().as_member(m),
			None => api(),
		})
	});
	let refresh = use_context_provider(|| Refresh(Signal::new(0)));
	let failure = use_context_provider(|| Failure(Signal::new(None)));
	let overview = use_resource(move || {
		let client = client.clone();
		async move {
			refresh.0();
			client.overview().await.map_err(shown)
		}
	});
	// `/me` is the caller whoever they act as: who a member tab is, and their balance, are read
	// off the members list, which only one who grants tokens may read
	let grants = may(Tokens::Grant);
	let unknown = tr.clone();
	let tokens = use_resource(move || {
		let unknown = unknown.clone();
		async move {
			refresh.0();
			let mine = api().me().await.map_err(shown)?.tokens;
			let whom = match tab.filter(|_| grants) {
				Some(m) => Some(
					api()
						.members()
						.await
						.map_err(shown)?
						.into_iter()
						.find(|x| x.id == m)
						.ok_or_else(|| t!(unknown, "workspace.no_member", "There is no member #{member}", member = m))?,
				),
				None => None,
			};
			Ok::<_, String>((mine, whom))
		}
	});

	let shell = SHELL;
	let gmails: Vec<GmailOverview> = match &*overview.read() {
		Some(Ok(g)) => g.clone(),
		Some(Err(e)) => return rsx! { div { class: "{shell} p-6 text-accent-error", "{e}" } },
		_ => return rsx! { div { class: "{shell} p-6 text-ink-soft", "Loading…" } },
	};
	let (tokens, whom) = match &*tokens.read() {
		Some(Ok((mine, whom))) => (
			match (tab, whom) {
				(None, _) => Some(*mine),
				(Some(_), Some(w)) => Some(TokensDto { balance: w.balance, ..*mine }),
				(Some(_), None) => None,
			},
			whom.clone(),
		),
		Some(Err(e)) => return rsx! { div { class: "{shell} p-6 text-accent-error", "{e}" } },
		None => (None, None),
	};
	let (picked, target) = match at {
		View::Home | View::Telegram | View::Tokens => (None, None),
		View::Gmail(gmail) => (Some(gmail), None),
		View::Place { gmail, target } => (Some(gmail), Some(target)),
	};
	// the first gmail until one is picked; a removed one falls back the same way
	let current = picked.filter(|id| gmails.iter().any(|g| g.gmail.id == *id)).or_else(|| gmails.first().map(|g| g.gmail.id));
	let scope = current.and_then(|id| gmails.iter().find(|g| g.gmail.id == id)).cloned();
	let acting = tab.map(|m| {
		let who = match whom {
			Some(w) if !w.name.is_empty() => format!("{} <{}>", w.name, w.email),
			Some(w) => w.email,
			None => format!("#{m}"),
		};
		t!(tr, "workspace.acting", "Viewing as {member} — actions apply to their account", member = who)
	});

	rsx! {
		div { class: "{shell}",
			Rail { gmails: gmails.clone(), current, at: at.clone(), tokens, tab }
			div { class: "flex min-h-0 min-w-0 flex-1 flex-col",
				if let Some(acting) = acting {
					div { class: "flex items-center gap-3 border-b border-border bg-accent-warn/15 px-6 py-2 text-accent-warn",
						span { class: "flex-1", "{acting}" }
						Link { class: "underline", to: Route::Mine { view: View::Home }, {t!(tr, "workspace.back", "Back to you")} }
					}
				}
				if let Some(e) = failure.0() {
					div { class: "border-b border-border bg-accent-error/15 px-6 py-2 text-accent-error", "{e}" }
				}
				match (at, scope) {
					(View::Telegram, _) => rsx! {
						telegram::Channels { gmails: gmails.iter().map(|g| g.gmail.clone()).collect::<Vec<_>>() }
					},
					(View::Tokens, _) => rsx! {
						tokens::Ledger { gmails: gmails.clone(), tokens, tab }
					},
					(_, None) => rsx! {
						div { class: "p-6 text-ink-soft", "Add the gmail your places are managed from, on the left." }
					},
					(_, Some(g)) => match target.and_then(|t| g.locations.iter().find(|l| l.target.id == t)) {
						Some(loc) => rsx! {
							board::Board { gmail: g.gmail.clone(), location: loc.clone(), tab }
						},
						None => rsx! { Places { scope: g, tab } },
					},
				}
			}
		}
	}
}

#[component]
fn Rail(gmails: Vec<GmailOverview>, current: Option<i64>, at: View, tokens: Option<TokensDto>, tab: Option<i64>) -> Element {
	let telegram = at == View::Telegram;
	let aside = telegram || at == View::Tokens;
	let mut adding = use_signal(String::new);
	let all_on = !gmails.is_empty() && gmails.iter().all(|g| g.gmail.enabled);
	let ids: Vec<i64> = gmails.iter().map(|g| g.gmail.id).collect();
	let row = "flex items-center gap-2 rounded-md px-3 py-2 text-left cursor-pointer hover:bg-hover";
	rsx! {
		nav { class: "flex w-60 shrink-0 flex-col overflow-y-auto border-r border-border bg-secondary",
			div { class: "flex items-center gap-2 px-4 py-4",
				span { class: "flex-1 text-[14px] font-semibold", "review_archive" }
				if let Some(t) = tokens {
					Link {
						class: if at == View::Tokens { "rounded-md px-2 py-0.5 bg-hover text-ink" } else { "rounded-md px-2 py-0.5 text-ink-soft hover:bg-hover" },
						to: Route::at(tab, View::Tokens),
						span { title: "+{t.daily}/day up to {t.cap}", "{t.balance} tokens" }
					}
				}
			}
			div { class: "flex items-center gap-1.5 pl-4 pr-2 pb-1 text-[11px] font-medium uppercase tracking-wide text-ink-soft",
				"Managing gmails"
				InfoTip {
					InfoTipTrigger { label: "What a managing gmail is" }
					InfoTipContent { class: "normal-case font-normal tracking-normal flex flex-col gap-2",
						p { "Full gmail address or any alias for it. Used only to group places; it doesn't affect any actions taken." }
						p { "If no managing account is connected, enter the email the place is on, or its shorthand." }
					}
				}
				div { class: "flex-1" }
				Switch {
					checked: all_on,
					disabled: ids.is_empty(),
					on_checked_change: move |on: bool| {
						let ids = ids.clone();
						act(move |c| async move {
							for id in ids {
								c.set_gmail_enabled(id, on).await?;
							}
							Ok(())
						});
					},
				}
			}
			div { class: "flex flex-col gap-0.5 px-2",
				for g in gmails {
					div { key: "{g.gmail.id}", class: "flex items-center gap-1",
						Link {
							class: format!(
								"{row} flex-1 min-w-0 {} {}",
								if current == Some(g.gmail.id) && !aside { "bg-hover text-ink" } else { "text-ink-soft" },
								if g.gmail.enabled { "" } else { "opacity-50" }
							),
							to: Route::at(tab, View::Gmail(g.gmail.id)),
							span { class: "truncate flex-1", "{g.gmail.gmail}" }
							uikit::Badge { variant: BadgeVariant::Secondary, "{g.locations.len()}" }
						}
						Switch {
							checked: g.gmail.enabled,
							on_checked_change: move |on: bool| {
								let id = g.gmail.id;
								act(move |c| async move { c.set_gmail_enabled(id, on).await });
							},
						}
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
			Link {
				class: if telegram { "{row} mx-2 mb-3 bg-hover text-ink" } else { "{row} mx-2 mb-3 text-ink-soft" },
				to: Route::at(tab, View::Telegram),
				"Telegram alerts"
			}
		}
	}
}

/// The gmail's places as cards, most screenshots over the last week first.
#[component]
fn Places(scope: GmailOverview, tab: Option<i64>) -> Element {
	let mut place = use_signal(String::new);
	let gmail = scope.gmail.id;
	rsx! {
		header { class: "flex h-14 shrink-0 items-center gap-3 border-b border-border px-6",
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
		main { class: "min-h-0 flex-1 overflow-y-auto p-6",
			if scope.locations.is_empty() {
				div { class: "text-ink-soft", "No places tracked under this gmail yet." }
			}
			div { class: "grid grid-cols-[repeat(auto-fill,minmax(320px,1fr))] gap-4",
				for loc in scope.locations {
					LocationCard {
						key: "{loc.target.id}",
						gmail,
						gmail_on: scope.gmail.enabled,
						loc: loc.clone(),
						to: Route::at(tab, View::Place { gmail, target: loc.target.id }),
					}
				}
			}
		}
	}
}

#[component]
fn LocationCard(gmail: i64, gmail_on: bool, loc: LocationSummary, to: Route) -> Element {
	let (dot, status) = match loc.last_run_status {
		Some(RunStatus::Ok) => ("bg-positive", "ok"),
		Some(RunStatus::Partial) => ("bg-accent-warn", "partial"),
		Some(RunStatus::Failed) => ("bg-accent-error", "failed"),
		None => ("bg-ink-soft", "never scanned"),
	};
	let last = loc.last_run_at.as_deref().map(|t| format!("last run {} · ", ago(t))).unwrap_or_default();
	let target = loc.target.id;
	rsx! {
		div { class: "relative",
		Link { class: if loc.enabled && gmail_on { "block text-left" } else { "block text-left opacity-50" }, to,
			Card { class: "gap-3 rounded-lg p-4 py-4 hover:border-ink-soft",
				div { class: "pr-12",
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
					Badge { tone: Tone::Ok,
						match loc.listed {
							Some(listed) => rsx! { span { title: "archived and still listed / listed on Google", "live {loc.live}/{listed}" } },
							None => rsx! { "live {loc.live}" },
						}
					}
					Badge { tone: Tone::Ok,
						span { title: "posted over the last 7 days, removed ones included", "new 7d +{loc.new_7d}" }
					}
					if let Some(post) = &loc.latest_post {
						Badge { tone: Tone::Ok,
							span { title: "owner's latest post, {ago(post.published_est.as_deref().unwrap_or(&post.first_seen))}:\n{post.text}", "posts 7d {loc.posts_7d}" }
						}
					}
					Badge { tone: if loc.responded < loc.live { Tone::Warn } else { Tone::Ok },
						span { title: "replied to by the owner / live", "responded {loc.responded}/{loc.live}" }
					}
					if loc.removed > 0 {
						Badge { tone: Tone::Bad, "removed {loc.removed}" }
					}
					if loc.reinstating > 0 {
						Badge { tone: Tone::Warn, "reinstating {loc.reinstating}" }
					}
					if loc.held {
						Badge { tone: Tone::Warn,
							span { title: "not scanned: every member tracking it is out of tokens", "out of tokens" }
						}
					}
				}
				div { class: "flex items-center gap-2 border-t border-border pt-3 text-[11px] text-ink-soft",
					span { class: "size-1.5 rounded-full {dot}" }
					"{last}{status}"
				}
			}
		}
		Switch {
			class: "absolute top-4 right-4",
			checked: loc.enabled,
			disabled: !gmail_on,
			on_checked_change: move |on: bool| act(move |c| async move { c.set_track_enabled(gmail, target, on).await }),
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
