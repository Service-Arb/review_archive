//! Where the member's events go on Telegram: a chat the archive's bot posts to.

use dioxus::prelude::*;
use ev_lib::uikit::{Button, ButtonVariant, Card, Input, Size};
use review_archive_client::dto::{Event, GmailDto, NewTgChannel, TgChannelDto};

use crate::{Api, Refresh, act, shown};

const EVENTS: [Event; 5] = [Event::ReviewNew, Event::ReviewGone, Event::ReviewChanged, Event::ReviewReappeared, Event::RunFailed];

#[component]
pub fn Channels(gmails: Vec<GmailDto>) -> Element {
	let Api(api) = use_context();
	let Refresh(refresh) = use_context();
	let channels = use_resource(move || {
		let api = api.clone();
		async move {
			refresh();
			api.tg_channels().await.map_err(shown)
		}
	});
	let channels = match &*channels.read() {
		Some(Ok(c)) => c.clone(),
		Some(Err(e)) => return rsx! { div { class: "p-6 text-accent-error", "{e}" } },
		_ => return rsx! { div { class: "p-6 text-ink-soft", "Loading…" } },
	};
	rsx! {
		header { class: "flex h-14 items-center border-b border-border px-6 font-medium", "Telegram alerts" }
		main { class: "flex flex-col gap-6 p-6",
			Card { class: "gap-0 overflow-hidden rounded-lg py-0",
				if channels.is_empty() {
					div { class: "p-4 text-ink-soft", "No channels yet." }
				}
				for ch in channels {
					Channel { key: "{ch.id}", ch: ch.clone(), gmails: gmails.clone() }
				}
			}
			AddChannel { gmails }
		}
	}
}

#[component]
fn Channel(ch: TgChannelDto, gmails: Vec<GmailDto>) -> Element {
	let scope = match ch.gmail_id {
		None => "all gmails".to_owned(),
		Some(id) => gmails.iter().find(|g| g.id == id).map_or_else(|| format!("gmail {id}"), |g| g.gmail.clone()),
	};
	let id = ch.id;
	rsx! {
		div { class: "flex items-center gap-4 border-b border-border px-4 py-3 last:border-b-0",
			span { class: "w-56 truncate font-mono", "{ch.destination}" }
			span { class: "w-48 truncate text-ink-soft", "{scope}" }
			div { class: "flex flex-1 flex-wrap gap-1",
				for e in ch.events {
					span { class: "rounded bg-muted px-2 py-0.5 font-mono text-[11px] text-ink-soft", "{e.as_ref()}" }
				}
			}
			Button {
				variant: ButtonVariant::Outline,
				size: Size::Sm,
				onclick: move |_| act(move |c| async move { c.test_tg_channel(id).await }),
				"Send test"
			}
			Button {
				variant: ButtonVariant::Ghost,
				size: Size::Sm,
				icon: true,
				class: "hover:text-accent-error",
				onclick: move |_| act(move |c| async move { c.delete_tg_channel(id).await }),
				"✕"
			}
		}
	}
}

#[component]
fn AddChannel(gmails: Vec<GmailDto>) -> Element {
	let mut destination = use_signal(String::new);
	let mut scope = use_signal(|| None::<i64>);
	let mut events = use_signal(|| vec![Event::ReviewNew, Event::ReviewGone]);
	rsx! {
		form {
			class: "flex max-w-xl flex-col gap-3 rounded-lg border border-border bg-card p-4",
			onsubmit: move |e| {
				e.prevent_default();
				let ch = NewTgChannel { destination: destination(), gmail_id: scope(), events: events() };
				act(move |c| async move { c.add_tg_channel(&ch).await.map(drop) });
				destination.set(String::new());
			},
			div { class: "text-[14px] font-semibold", "Add channel" }
			Input {
				class: "font-mono",
				placeholder: "@channel, -100…, or <group>/<topic>",
				value: destination(),
				oninput: move |e: FormEvent| destination.set(e.value()),
			}
			div { class: "text-[11px] text-ink-soft", "Add the archive's bot to the chat, allowed to post, then paste the chat here." }
			select {
				class: "h-9 rounded-[var(--control-radius)] border border-input bg-transparent px-3 text-ink",
				onchange: move |e| scope.set(e.value().parse().ok()),
				option { value: "", "All gmails" }
				for g in gmails {
					option { key: "{g.id}", value: "{g.id}", "{g.gmail}" }
				}
			}
			div { class: "flex flex-wrap gap-3",
				for ev in EVENTS {
					label { key: "{ev.as_ref()}", class: "flex items-center gap-1.5 font-mono text-[11px] text-ink-soft",
						input {
							r#type: "checkbox",
							class: "accent-primary",
							checked: events().contains(&ev),
							onchange: move |_| {
								let mut on = events();
								if let Some(i) = on.iter().position(|x| *x == ev) {
									on.remove(i);
								} else {
									on.push(ev);
								}
								events.set(on);
							},
						}
						"{ev.as_ref()}"
					}
				}
			}
			Button { class: "self-start", "Add" }
		}
	}
}
