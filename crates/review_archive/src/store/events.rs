//! Events into the outbox, inside the transaction that caused them: an event is recorded
//! exactly when what it reports is. Hooks get every target's; a member's Telegram channel
//! gets those of the targets the member tracks (under its gmail, when it names one).

use std::collections::{HashMap, HashSet, hash_map::Entry};

use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{
	ReviewId,
	dto::{Event, EventPayload},
	fmt_ts,
};
use sqlx::SqliteConnection;

use super::{RunId, review_by_id, run_by_id, webhooks::parse_events};

/// Where an outbox row goes.
#[derive(Clone, Copy)]
enum Sink {
	Webhook(i64),
	Telegram(i64),
}

struct Channel {
	id: i64,
	events: HashSet<Event>,
	targets: HashSet<i64>,
}

/// The hooks and channels as they were when the transaction began, and what each wants.
pub(super) struct Emitter {
	hooks: Vec<(i64, HashSet<Event>)>,
	channels: Vec<Channel>,
	now: Timestamp,
}

impl Emitter {
	pub(super) async fn load(tx: &mut SqliteConnection, now: Timestamp) -> eyre::Result<Self> {
		let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, events FROM webhooks").fetch_all(&mut *tx).await.wrap_err("loading webhooks")?;
		let hooks = rows
			.into_iter()
			.map(|(id, events)| Ok((id, parse_events(id, &events)?.into_iter().collect())))
			.collect::<eyre::Result<_>>()?;
		let rows: Vec<(i64, String, i64)> = sqlx::query_as(
			"SELECT c.id, c.events, k.target_id FROM tg_channels c
			 JOIN managing_gmails g ON g.person_id = c.person_id AND (c.managing_gmail_id IS NULL OR c.managing_gmail_id = g.id)
			 JOIN tracks k ON k.managing_gmail_id = g.id",
		)
		.fetch_all(&mut *tx)
		.await
		.wrap_err("loading Telegram channels")?;
		let mut channels: HashMap<i64, Channel> = HashMap::new();
		for (id, events, target) in rows {
			let ch = match channels.entry(id) {
				Entry::Occupied(o) => o.into_mut(),
				Entry::Vacant(v) => v.insert(Channel {
					id,
					events: parse_events(id, &events)?.into_iter().collect(),
					targets: HashSet::new(),
				}),
			};
			ch.targets.insert(target);
		}
		Ok(Self {
			hooks,
			channels: channels.into_values().collect(),
			now,
		})
	}

	fn wanted(&self, event: Event) -> bool {
		self.hooks.iter().any(|(_, e)| e.contains(&event)) || self.channels.iter().any(|c| c.events.contains(&event))
	}

	fn subscribers(&self, event: Event, target: i64) -> Vec<Sink> {
		let hooks = self.hooks.iter().filter(|(_, e)| e.contains(&event)).map(|(id, _)| Sink::Webhook(*id));
		let channels = self
			.channels
			.iter()
			.filter(|c| c.events.contains(&event) && c.targets.contains(&target))
			.map(|c| Sink::Telegram(c.id));
		hooks.chain(channels).collect()
	}

	/// A `review.*` event about this review, as it is now in the transaction.
	pub(super) async fn review(&mut self, tx: &mut SqliteConnection, event: Event, id: ReviewId) -> eyre::Result<()> {
		if !self.wanted(event) {
			return Ok(());
		}
		let review = review_by_id(&mut *tx, id).await?.ok_or_else(|| eyre::eyre!("review {id} vanished inside its own transaction"))?;
		let payload = EventPayload {
			event,
			occurred_at: fmt_ts(self.now),
			target_id: review.target_id,
			review: Some(review),
			run: None,
		};
		self.insert(tx, &payload).await
	}

	/// `run.failed` for this run, as just recorded.
	pub(super) async fn run_failed(&mut self, tx: &mut SqliteConnection, run: RunId) -> eyre::Result<()> {
		if !self.wanted(Event::RunFailed) {
			return Ok(());
		}
		let run = run_by_id(&mut *tx, run).await?.ok_or_else(|| eyre::eyre!("run {} vanished inside its own transaction", run.0))?;
		let payload = EventPayload {
			event: Event::RunFailed,
			occurred_at: fmt_ts(self.now),
			target_id: run.target_id,
			review: None,
			run: Some(run),
		};
		self.insert(tx, &payload).await
	}

	async fn insert(&self, tx: &mut SqliteConnection, payload: &EventPayload) -> eyre::Result<()> {
		let body = serde_json::to_string(payload)?;
		let now = fmt_ts(self.now);
		for sink in self.subscribers(payload.event, payload.target_id) {
			let (hook, channel) = match sink {
				Sink::Webhook(id) => (Some(id), None),
				Sink::Telegram(id) => (None, Some(id)),
			};
			sqlx::query("INSERT INTO webhook_deliveries (webhook_id, tg_channel_id, event, payload, created_at, next_attempt_at) VALUES (?, ?, ?, ?, ?, ?)")
				.bind(hook)
				.bind(channel)
				.bind(payload.event.as_ref())
				.bind(&body)
				.bind(&now)
				.bind(&now)
				.execute(&mut *tx)
				.await
				.wrap_err("queueing a delivery")?;
		}
		Ok(())
	}
}
