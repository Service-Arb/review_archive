//! The dashboard: a member's managing gmails, the places each tracks, and per place a board
//! of its reviews — snapshotted, removed, reinstating. Served by the archive under `/mfe/`,
//! so the API is the bundle's own origin, and the browser's `va_access` cookie signs its calls.
//! An admin gets tabs: their own dashboard, and any member's, acting as them.

#![cfg(target_arch = "wasm32")]
#![allow(clippy::useless_format)] // rsx! lowers every "{x}" to a format!

mod board;
mod telegram;
mod tokens;

use dioxus::prelude::*;
use ev_lib::{
	i18n::Messages,
	mfe::bundle_origin,
	uikit::{
		self, BadgeVariant, Button, ButtonVariant, Card, CommandDialog, CommandEmpty, CommandInput, CommandItem, CommandList, InfoTip, InfoTipContent, InfoTipTrigger, Input, Size, Switch,
		Tabs, TabsList, TabsTrigger,
	},
};
use review_archive_client::{
	Client,
	dto::{BalanceChange, GmailOverview, LocationSummary, MemberDto, NewTrack, RunStatus, TokensChange, TokensDto},
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

/// Where the dashboard is, as its URL says: a view of the caller's own, or of the member an
/// admin's tab acts as.
#[derive(Clone, Debug, PartialEq, Routable)]
#[rustfmt::skip]
enum Route {
	#[layout(Shell)]
		#[route("/members/:member/:..view", TheirView)]
		Theirs { member: String, view: View },
		#[route("/:..view", MyView)]
		Mine { view: View },
}

impl Route {
	fn at(member: Option<String>, view: View) -> Self {
		match member {
			Some(member) => Self::Theirs { member, view },
			None => Self::Mine { view },
		}
	}

	fn member(&self) -> Option<&str> {
		match self {
			Self::Theirs { member, .. } => Some(member),
			Self::Mine { .. } => None,
		}
	}

	fn view(&self) -> &View {
		match self {
			Self::Theirs { view, .. } | Self::Mine { view } => view,
		}
	}
}

/// Each tab's last route, by its member: what a hidden tab keeps showing, and where going
/// back to it lands.
#[derive(Clone, Copy)]
struct Views(Signal<std::collections::HashMap<Option<String>, Route>>);

/// The routes render nothing themselves — every tab stays mounted under [`Shell`] — they
/// only note where their tab is.
fn remember(route: Route) -> Element {
	let Views(mut views) = use_context();
	use_effect(use_reactive!(|route| {
		views.write().insert(route.member().map(str::to_owned), route);
	}));
	rsx! {}
}

#[component]
fn MyView(view: View) -> Element {
	remember(Route::Mine { view })
}

#[component]
fn TheirView(member: String, view: View) -> Element {
	remember(Route::Theirs { member, view })
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

const SHELL: &str = "flex min-h-0 flex-1 bg-background text-ink text-[13px] font-sans";

#[component]
fn Dashboard() -> Element {
	rsx! { Router::<Route> {} }
}

#[component]
fn Shell() -> Element {
	use_context_provider(|| Views(Signal::new(Default::default())));
	let route = use_route::<Route>();
	let me = use_resource(|| async { api().me().await.map_err(shown) });
	let body = match &*me.read() {
		Some(Ok(me)) if me.admin => rsx! { Admin { email: me.email.clone() } },
		Some(Ok(_)) => rsx! { Workspace { view: route } },
		Some(Err(e)) => rsx! { div { class: "{SHELL} p-6 text-accent-error", "{e}" } },
		None => rsx! { div { class: "{SHELL} p-6 text-ink-soft", "Loading…" } },
	};
	rsx! {
		div { class: "flex h-dvh flex-col",
			{body}
			Outlet::<Route> {}
		}
	}
}

/// "You", then a tab per member opened; every tab stays mounted, so switching keeps where
/// each one was. The active tab is the URL's `member`.
#[component]
fn Admin(email: String) -> Element {
	let route = use_route::<Route>();
	let Views(mut views) = use_context();
	let active = route.member().map(str::to_owned);
	let mut opened = use_signal(Vec::<String>::new);
	let mut labels = use_signal(std::collections::HashMap::<String, String>::new);
	let mut picking = use_signal(|| false);
	// a link to a member's view opens their tab
	use_effect(use_reactive!(|active| {
		if let Some(m) = active
			&& !opened.peek().contains(&m)
		{
			opened.write().push(m);
		}
	}));
	let mut tabs = opened();
	if let Some(m) = &active
		&& !tabs.contains(m)
	{
		tabs.push(m.clone());
	}
	// where a tab is: the URL for the active one, else where it was left; a tab never visited starts at its home
	let view_of = move |m: Option<String>| match views.read().get(&m) {
		Some(r) => r.clone(),
		None => Route::at(m, View::Home),
	};
	let shown = |m: Option<&str>| match active.as_deref() == m {
		true => "flex min-h-0 flex-1",
		false => "hidden",
	};
	rsx! {
		div { class: "flex min-h-0 flex-1 flex-col bg-background text-ink text-[13px] font-sans",
			Tabs {
				// the kit's tabs are keyed by string: "" is the admin's own
				value: active.clone().unwrap_or_default(),
				on_value_change: move |m: String| {
					navigator().push(view_of((!m.is_empty()).then_some(m)));
				},
				class: "border-b border-border bg-secondary px-2 py-1.5",
				div { class: "flex items-center gap-1",
					TabsList { class: "bg-transparent",
						TabsTrigger { value: "", "You" }
						for m in tabs.clone() {
							div { key: "{m}", class: "flex items-center",
								TabsTrigger { value: m.clone(), {labels.read().get(&m).cloned().unwrap_or_else(|| m.clone())} }
								Button {
									variant: ButtonVariant::Ghost,
									size: Size::Xs,
									icon: true,
									r#type: "button",
									onclick: move |_| {
										opened.write().retain(|t| *t != m);
										views.write().remove(&Some(m.clone()));
										if router().current::<Route>().member() == Some(m.as_str()) {
											navigator().push(view_of(None));
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
						class: "text-ink-soft text-base",
						onclick: move |_| picking.set(true),
						"+"
					}
				}
			}
			div { class: shown(None),
				Workspace { view: if active.is_none() { route.clone() } else { view_of(None) } }
			}
			for m in tabs {
				div { key: "{m}", class: shown(Some(&m)),
					Workspace { view: if active.as_ref() == Some(&m) { route.clone() } else { view_of(Some(m.clone())) } }
				}
			}
			if picking() {
				Picker {
					on_pick: move |m: MemberDto| {
						picking.set(false);
						let tab = (m.email != email).then(|| m.email.clone());
						if let Some(name) = m.username {
							labels.write().insert(m.email, name);
						}
						navigator().push(view_of(tab));
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
								span { class: "flex-1 truncate text-ink-soft", "{m.email}" }
								Balance { member: m.email.clone(), balance: m.balance }
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

/// A member's balance in the picker, and an admin's change of it.
#[component]
fn Balance(member: String, balance: i64) -> Element {
	let mut balance = use_signal(|| balance);
	let mut amount = use_signal(String::new);
	let mut failure = use_signal(|| None::<String>);
	let mut change = move |to: fn(i64) -> BalanceChange| {
		let n = match amount().trim().parse::<i64>() {
			Ok(n) => n,
			Err(e) => return failure.set(Some(format!("{:?}: {e}", amount()))),
		};
		let member = member.clone();
		spawn(async move {
			match api().change_tokens(&member, &TokensChange { change: to(n), note: None }).await {
				Ok(t) => {
					balance.set(t.balance);
					amount.set(String::new());
					failure.set(None);
				}
				Err(e) => failure.set(Some(shown(e))),
			}
		});
	};
	let mut set = change.clone();
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

/// One tab's dashboard at `view`: the signed-in person's own, or (its `member`) the one an
/// admin acts as.
#[component]
fn Workspace(view: Route) -> Element {
	let tab = view.member().map(str::to_owned);
	let member = tab.clone();
	let Api(client) = use_context_provider(|| {
		Api(match &member {
			Some(m) => api().as_member(m.clone()),
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
	// `/me` is the caller whoever they act as: a member tab's balance is read off the members list
	let acting = tab.clone();
	let tokens = use_resource(move || {
		let member = acting.clone();
		async move {
			refresh.0();
			let me = api().me().await.map_err(shown)?;
			let balance = match member {
				None => me.tokens.balance,
				Some(m) =>
					api()
						.members()
						.await
						.map_err(shown)?
						.into_iter()
						.find(|x| x.email == m)
						.ok_or_else(|| format!("{m} is not a member"))?
						.balance,
			};
			Ok::<_, String>(TokensDto { balance, ..me.tokens })
		}
	});

	let shell = SHELL;
	let gmails: Vec<GmailOverview> = match &*overview.read() {
		Some(Ok(g)) => g.clone(),
		Some(Err(e)) => return rsx! { div { class: "{shell} p-6 text-accent-error", "{e}" } },
		_ => return rsx! { div { class: "{shell} p-6 text-ink-soft", "Loading…" } },
	};
	let tokens = match &*tokens.read() {
		Some(Ok(t)) => Some(*t),
		Some(Err(e)) => return rsx! { div { class: "{shell} p-6 text-accent-error", "{e}" } },
		None => None,
	};
	let at = view.view().clone();
	let (picked, target) = match at {
		View::Home | View::Telegram | View::Tokens => (None, None),
		View::Gmail(gmail) => (Some(gmail), None),
		View::Place { gmail, target } => (Some(gmail), Some(target)),
	};
	// the first gmail until one is picked; a removed one falls back the same way
	let current = picked.filter(|id| gmails.iter().any(|g| g.gmail.id == *id)).or_else(|| gmails.first().map(|g| g.gmail.id));
	let scope = current.and_then(|id| gmails.iter().find(|g| g.gmail.id == id)).cloned();

	rsx! {
		div { class: "{shell}",
			Rail { gmails: gmails.clone(), current, at: at.clone(), tokens, tab: tab.clone() }
			div { class: "flex min-h-0 min-w-0 flex-1 flex-col",
				if let Some(m) = &member {
					div { class: "border-b border-border bg-accent-warn/15 px-6 py-2 text-accent-warn", "Viewing as {m} — actions apply to their account" }
				}
				if let Some(e) = failure.0() {
					div { class: "border-b border-border bg-accent-error/15 px-6 py-2 text-accent-error", "{e}" }
				}
				match (at, scope) {
					(View::Telegram, _) => rsx! {
						telegram::Channels { gmails: gmails.iter().map(|g| g.gmail.clone()).collect::<Vec<_>>() }
					},
					(View::Tokens, _) => rsx! {
						tokens::Ledger { gmails: gmails.clone(), tokens, tab: tab.clone() }
					},
					(_, None) => rsx! {
						div { class: "p-6 text-ink-soft", "Add the gmail your places are managed from, on the left." }
					},
					(_, Some(g)) => match target.and_then(|t| g.locations.iter().find(|l| l.target.id == t)) {
						Some(loc) => rsx! {
							board::Board { gmail: g.gmail.clone(), location: loc.clone(), tab: tab.clone() }
						},
						None => rsx! { Places { scope: g, tab: tab.clone() } },
					},
				}
			}
		}
	}
}

#[component]
fn Rail(gmails: Vec<GmailOverview>, current: Option<i64>, at: View, tokens: Option<TokensDto>, tab: Option<String>) -> Element {
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
						to: Route::at(tab.clone(), View::Tokens),
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
							to: Route::at(tab.clone(), View::Gmail(g.gmail.id)),
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
fn Places(scope: GmailOverview, tab: Option<String>) -> Element {
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
						to: Route::at(tab.clone(), View::Place { gmail, target: loc.target.id }),
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
