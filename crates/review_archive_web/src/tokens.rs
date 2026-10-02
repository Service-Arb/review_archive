//! What the member's balance holds, and every move of it.

use dioxus::prelude::*;
use ev_lib::uikit::Card;
use review_archive_client::dto::{GmailOverview, TokenKind, TokensDto};

use crate::{Api, Refresh, Route, View, ago, shown};

#[component]
pub fn Ledger(gmails: Vec<GmailOverview>, tokens: Option<TokensDto>, tab: Option<String>) -> Element {
	let Api(api) = use_context();
	let Refresh(refresh) = use_context();
	let ledger = use_resource(move || {
		let api = api.clone();
		async move {
			refresh();
			api.ledger().await.map_err(shown)
		}
	});
	let ledger = match &*ledger.read() {
		Some(Ok(l)) => l.clone(),
		Some(Err(e)) => return rsx! { div { class: "p-6 text-accent-error", "{e}" } },
		_ => return rsx! { div { class: "p-6 text-ink-soft", "Loading…" } },
	};
	let gmail_of = |target: i64| gmails.iter().find(|g| g.locations.iter().any(|l| l.target.id == target)).map(|g| g.gmail.id);
	rsx! {
		header { class: "flex h-14 shrink-0 items-center gap-3 border-b border-border px-6",
			span { class: "font-medium", "Tokens" }
			if let Some(t) = tokens {
				span { class: "text-ink-soft", "{t.balance} held · +{t.daily}/day up to {t.cap}" }
			}
		}
		main { class: "min-h-0 flex-1 overflow-y-auto p-6",
			Card { class: "gap-0 overflow-hidden rounded-lg py-0",
				if ledger.is_empty() {
					div { class: "p-4 text-ink-soft", "No moves yet." }
				}
				for (i, e) in ledger.into_iter().enumerate() {
					div { key: "{i}", class: "flex items-center gap-4 border-b border-border px-4 py-2 last:border-b-0",
						span { class: "w-20 shrink-0 text-ink-soft", title: "{e.at}", "{ago(&e.at)}" }
						span { class: "w-20 shrink-0 font-mono text-[11px] text-ink-soft", "{e.kind}" }
						span { class: if e.delta < 0 { "w-16 shrink-0 text-right font-mono text-accent-error" } else { "w-16 shrink-0 text-right font-mono text-positive" },
							if e.delta > 0 { "+{e.delta}" } else { "{e.delta}" }
						}
						span { class: "min-w-0 flex-1 truncate",
							match (e.kind, e.target_id, e.target_label) {
								(TokenKind::Charge, Some(target), Some(label)) => match gmail_of(target) {
									Some(gmail) => rsx! { Link { class: "hover:underline", to: Route::at(tab.clone(), View::Place { gmail, target }), "{label}" } },
									None => rsx! { "{label}" },
								},
								_ => rsx! {},
							}
							if let Some(note) = e.note {
								span { class: "text-ink-soft", " {note}" }
							}
							if let Some(by) = e.by {
								span { class: "text-ink-soft", " — {by}" }
							}
						}
					}
				}
			}
		}
	}
}
