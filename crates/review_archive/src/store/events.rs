//! Webhook events into the outbox, inside the transaction that caused them: an event is
//! recorded exactly when what it reports is.

use std::collections::HashSet;

use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{
	ReviewId,
	dto::{Event, EventPayload},
	fmt_ts,
};
use sqlx::SqliteConnection;

use super::{RunId, review_by_id, run_by_id, webhooks::parse_events};

/// The hooks as they were when the transaction began, and what each wants.
pub(super) struct Emitter {
	hooks: Vec<(i64, HashSet<Event>)>,
	now: Timestamp,
}

impl Emitter {
	pub(super) async fn load(tx: &mut SqliteConnection, now: Timestamp) -> eyre::Result<Self> {
		let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, events FROM webhooks").fetch_all(&mut *tx).await.wrap_err("loading webhooks")?;
		let hooks = rows
			.into_iter()
			.map(|(id, events)| Ok((id, parse_events(id, &events)?.into_iter().collect())))
			.collect::<eyre::Result<_>>()?;
		Ok(Self { hooks, now })
	}

	fn subscribers(&self, event: Event) -> Vec<i64> {
		self.hooks.iter().filter(|(_, e)| e.contains(&event)).map(|(id, _)| *id).collect()
	}

	/// A `review.*` event about this review, as it is now in the transaction.
	pub(super) async fn review(&mut self, tx: &mut SqliteConnection, event: Event, id: ReviewId) -> eyre::Result<()> {
		let hooks = self.subscribers(event);
		if hooks.is_empty() {
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
		self.insert(tx, &hooks, &payload).await
	}

	/// `run.failed` for this run, as just recorded.
	pub(super) async fn run_failed(&mut self, tx: &mut SqliteConnection, run: RunId) -> eyre::Result<()> {
		let hooks = self.subscribers(Event::RunFailed);
		if hooks.is_empty() {
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
		self.insert(tx, &hooks, &payload).await
	}

	async fn insert(&self, tx: &mut SqliteConnection, hooks: &[i64], payload: &EventPayload) -> eyre::Result<()> {
		let body = serde_json::to_string(payload)?;
		let now = fmt_ts(self.now);
		for hook in hooks {
			sqlx::query("INSERT INTO webhook_deliveries (webhook_id, event, payload, created_at, next_attempt_at) VALUES (?, ?, ?, ?, ?)")
				.bind(hook)
				.bind(payload.event.as_ref())
				.bind(&body)
				.bind(&now)
				.bind(&now)
				.execute(&mut *tx)
				.await
				.wrap_err("queueing a webhook delivery")?;
		}
		Ok(())
	}
}
