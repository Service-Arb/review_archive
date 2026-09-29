//! Webhooks and the outbox, which also carries members' Telegram channels' events.

use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{
	dto::{Event, NewWebhook, WebhookDto},
	fmt_ts,
};
use sqlx::FromRow;

use super::Store;

/// A delivery that is due.
#[derive(Clone, Debug)]
pub struct Delivery {
	/// Its id.
	pub id: i64,
	/// Where it goes.
	pub to: Recipient,
	/// `review.new`, …
	pub event: String,
	/// The JSON body, exactly as signed.
	pub payload: String,
	/// Tries so far.
	pub attempts: i64,
}

/// Where a delivery goes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recipient {
	/// A hook: POSTed, signed.
	Webhook {
		/// The hook.
		id: i64,
		/// Where to POST.
		url: String,
		/// The HMAC key.
		secret: String,
	},
	/// A member's Telegram channel, through the archive's bot.
	Telegram {
		/// The channel.
		id: i64,
		/// As the member pasted it.
		destination: String,
	},
}

#[derive(FromRow)]
struct DeliveryRow {
	id: i64,
	webhook_id: Option<i64>,
	url: Option<String>,
	secret: Option<String>,
	tg_channel_id: Option<i64>,
	destination: Option<String>,
	event: String,
	payload: String,
	attempts: i64,
}

impl From<DeliveryRow> for Delivery {
	fn from(r: DeliveryRow) -> Self {
		let to = match (r.webhook_id, r.url, r.secret, r.tg_channel_id, r.destination) {
			(Some(id), Some(url), Some(secret), None, None) => Recipient::Webhook { id, url, secret },
			(None, None, None, Some(id), Some(destination)) => Recipient::Telegram { id, destination },
			_ => unreachable!("a CHECK makes a delivery name exactly one hook or channel, and each cascades away with its row"),
		};
		Self {
			id: r.id,
			to,
			event: r.event,
			payload: r.payload,
			attempts: r.attempts,
		}
	}
}

#[derive(FromRow)]
struct WebhookRow {
	id: i64,
	url: String,
	events: String,
	created_at: String,
}

/// A hook's events, as stored: a JSON array of their names.
pub(super) fn parse_events(hook: i64, json: &str) -> eyre::Result<Vec<Event>> {
	serde_json::from_str(json).wrap_err_with(|| format!("webhook {hook} has unreadable events"))
}

impl TryFrom<WebhookRow> for WebhookDto {
	type Error = eyre::Report;

	fn try_from(r: WebhookRow) -> eyre::Result<Self> {
		Ok(Self {
			id: r.id,
			url: r.url,
			events: parse_events(r.id, &r.events)?,
			created_at: r.created_at,
		})
	}
}

impl Store {
	/// Adds a hook; it gets the events recorded from now on.
	pub async fn add_webhook(&self, hook: &NewWebhook, now: Timestamp) -> eyre::Result<WebhookDto> {
		let row: WebhookRow = sqlx::query_as("INSERT INTO webhooks (url, events, secret, created_at) VALUES (?, ?, ?, ?) RETURNING id, url, events, created_at")
			.bind(&hook.url)
			.bind(serde_json::to_string(&hook.events)?)
			.bind(&hook.secret)
			.bind(fmt_ts(now))
			.fetch_one(&self.pool)
			.await
			.wrap_err("adding a webhook")?;
		row.try_into()
	}

	/// Every hook, without secrets.
	pub async fn webhooks(&self) -> eyre::Result<Vec<WebhookDto>> {
		let rows: Vec<WebhookRow> = sqlx::query_as("SELECT id, url, events, created_at FROM webhooks ORDER BY id")
			.fetch_all(&self.pool)
			.await
			.wrap_err("listing webhooks")?;
		rows.into_iter().map(WebhookDto::try_from).collect()
	}

	/// Removes a hook and what it was still owed. `false` when there was none.
	pub async fn delete_webhook(&self, id: i64) -> eyre::Result<bool> {
		let done = sqlx::query("DELETE FROM webhooks WHERE id = ?")
			.bind(id)
			.execute(&self.pool)
			.await
			.wrap_err("deleting a webhook")?;
		Ok(done.rows_affected() == 1)
	}

	/// Deliveries due at `now`, oldest first.
	pub async fn due_deliveries(&self, now: Timestamp, limit: u32) -> eyre::Result<Vec<Delivery>> {
		let rows: Vec<DeliveryRow> = sqlx::query_as(
			"SELECT d.id, d.webhook_id, w.url, w.secret, d.tg_channel_id, c.destination, d.event, d.payload, d.attempts
			 FROM webhook_deliveries d
			 LEFT JOIN webhooks w ON w.id = d.webhook_id
			 LEFT JOIN tg_channels c ON c.id = d.tg_channel_id
			 WHERE d.delivered_at IS NULL AND d.failed_at IS NULL AND d.next_attempt_at <= ?
			 ORDER BY d.id LIMIT ?",
		)
		.bind(fmt_ts(now))
		.bind(limit)
		.fetch_all(&self.pool)
		.await
		.wrap_err("loading due deliveries")?;
		Ok(rows.into_iter().map(Delivery::from).collect())
	}

	/// A delivery the receiver acknowledged.
	pub async fn delivered(&self, id: i64, now: Timestamp) -> eyre::Result<()> {
		sqlx::query("UPDATE webhook_deliveries SET delivered_at = ?, attempts = attempts + 1, last_error = NULL WHERE id = ?")
			.bind(fmt_ts(now))
			.bind(id)
			.execute(&self.pool)
			.await
			.wrap_err("recording a delivery")?;
		Ok(())
	}

	/// A failed try: retried at `retry_at`, or given up on when `None`.
	pub async fn delivery_failed(&self, id: i64, error: &str, retry_at: Option<Timestamp>, now: Timestamp) -> eyre::Result<()> {
		sqlx::query(
			"UPDATE webhook_deliveries SET attempts = attempts + 1, last_error = ?, next_attempt_at = COALESCE(?, next_attempt_at),
			                               failed_at = CASE WHEN ? IS NULL THEN ? END
			 WHERE id = ?",
		)
		.bind(error)
		.bind(retry_at.map(fmt_ts))
		.bind(retry_at.map(fmt_ts))
		.bind(fmt_ts(now))
		.bind(id)
		.execute(&self.pool)
		.await
		.wrap_err("recording a failed delivery")?;
		Ok(())
	}

	/// `(pending, delivered, failed)` deliveries of a hook.
	pub async fn delivery_counts(&self, webhook: i64) -> eyre::Result<(i64, i64, i64)> {
		sqlx::query_as(
			"SELECT COALESCE(SUM(delivered_at IS NULL AND failed_at IS NULL), 0), COALESCE(SUM(delivered_at IS NOT NULL), 0), COALESCE(SUM(failed_at IS NOT NULL), 0)
			 FROM webhook_deliveries WHERE webhook_id = ?",
		)
		.bind(webhook)
		.fetch_one(&self.pool)
		.await
		.wrap_err("counting deliveries")
	}
}
