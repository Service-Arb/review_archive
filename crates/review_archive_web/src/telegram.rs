//! Where the member's events go on Telegram: a chat the archive's bot posts to.

use dioxus::prelude::*;
use review_archive_client::dto::{Event, GmailDto, NewTgChannel, TgChannelDto};

use crate::{Refresh, Token, act, api};

const EVENTS: [Event; 5] = [Event::ReviewNew, Event::ReviewGone, Event::ReviewChanged, Event::ReviewReappeared, Event::RunFailed];

#[component]
pub fn Channels(gmails: Vec<GmailDto>) -> Element {
	let Token(token) = use_context();
	let Refresh(refresh) = use_context();
	let channels = use_resource(move || async move {
		refresh();
		let t = token()?;
		Some(api(&t).tg_channels().await.map_err(|e| e.to_string()))
	});
	let channels = match &*channels.read() {
		Some(Some(Ok(c))) => c.clone(),
		Some(Some(Err(e))) => return rsx! { div { class: "p-6 text-bad", "{e}" } },
		_ => return rsx! { div { class: "p-6 text-muted", "Loading…" } },
	};
	rsx! {
		header { class: "flex h-14 items-center border-b border-line px-6 font-medium", "Telegram alerts" }
		main { class: "flex flex-col gap-6 p-6",
			div { class: "overflow-hidden rounded-lg border border-line",
				if channels.is_empty() {
					div { class: "bg-panel p-4 text-muted", "No channels yet." }
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
		div { class: "flex items-center gap-4 border-b border-line bg-panel px-4 py-3 last:border-b-0",
			span { class: "w-56 truncate font-mono", "{ch.destination}" }
			span { class: "w-48 truncate text-muted", "{scope}" }
			div { class: "flex flex-1 flex-wrap gap-1",
				for e in ch.events {
					span { class: "rounded bg-raised px-2 py-0.5 font-mono text-[11px] text-muted", "{e.as_ref()}" }
				}
			}
			button {
				class: "rounded-md border border-line bg-raised px-3 py-1.5 hover:border-faint",
				onclick: move |_| act(move |c| async move { c.test_tg_channel(id).await }),
				"Send test"
			}
			button {
				class: "px-2 text-faint hover:text-bad",
				title: "Remove",
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
			class: "flex max-w-xl flex-col gap-3 rounded-lg border border-line bg-panel p-4",
			onsubmit: move |e| {
				e.prevent_default();
				let ch = NewTgChannel { destination: destination(), gmail_id: scope(), events: events() };
				act(move |c| async move { c.add_tg_channel(&ch).await.map(drop) });
				destination.set(String::new());
			},
			div { class: "text-[14px] font-semibold", "Add channel" }
			input {
				class: "rounded-md border border-line bg-page px-2.5 py-2 font-mono text-fg placeholder:text-faint focus:border-accent focus:outline-none",
				placeholder: "@channel, -100…, or <group>/<topic>",
				value: "{destination}",
				oninput: move |e| destination.set(e.value()),
			}
			div { class: "text-[11px] text-faint", "Add the archive's bot to the chat, allowed to post, then paste the chat here." }
			select {
				class: "rounded-md border border-line bg-page px-2.5 py-2 text-fg",
				onchange: move |e| scope.set(e.value().parse().ok()),
				option { value: "", "All gmails" }
				for g in gmails {
					option { key: "{g.id}", value: "{g.id}", "{g.gmail}" }
				}
			}
			div { class: "flex flex-wrap gap-3",
				for ev in EVENTS {
					label { key: "{ev.as_ref()}", class: "flex items-center gap-1.5 font-mono text-[11px] text-muted",
						input {
							r#type: "checkbox",
							class: "accent-accent",
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
			button { class: "self-start rounded-md bg-accent px-3 py-1.5 font-medium text-on-accent", "Add" }
		}
	}
}
