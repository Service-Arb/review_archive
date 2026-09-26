//! Webhooks and their outbox.

use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{
	dto::{Event, NewWebhook, WebhookDto},
	fmt_ts,
};
use sqlx::FromRow;

use super::Store;

/// A delivery that is due.
#[derive(Clone, Debug, FromRow)]
pub struct Delivery {
	/// Its id.
	pub id: i64,
	/// The hook.
	pub webhook_id: i64,
	/// Where to POST.
	pub url: String,
	/// The HMAC key.
	pub secret: String,
	/// `review.new`, …
	pub event: String,
	/// The JSON body, exactly as signed.
	pub payload: String,
	/// Tries so far.
	pub attempts: i64,
}

#[derive(FromRow)]
struct WebhookRow {
	id: i64,
	url: String,
	events: String,
	created_at: String,
}

impl TryFrom<WebhookRow> for WebhookDto {
	type Error = eyre::Report;

	fn try_from(r: WebhookRow) -> eyre::Result<Self> {
		Ok(Self {
			id: r.id,
			url: r.url,
			events: serde_json::from_str::<Vec<Event>>(&r.events).wrap_err_with(|| format!("webhook {} has unreadable events", r.id))?,
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
		sqlx::query_as(
			"SELECT d.id, d.webhook_id, w.url, w.secret, d.event, d.payload, d.attempts
			 FROM webhook_deliveries d JOIN webhooks w ON w.id = d.webhook_id
			 WHERE d.delivered_at IS NULL AND d.failed_at IS NULL AND d.next_attempt_at <= ?
			 ORDER BY d.id LIMIT ?",
		)
		.bind(fmt_ts(now))
		.bind(limit)
		.fetch_all(&self.pool)
		.await
		.wrap_err("loading due deliveries")
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
