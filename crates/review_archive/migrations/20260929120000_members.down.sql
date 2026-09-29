-- Drops every member's gmails, tracks, appeal history and Telegram channels, and what was
-- still owed to those channels; the archive and the hooks are untouched.
CREATE TABLE webhook_deliveries_old (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	webhook_id INTEGER NOT NULL REFERENCES webhooks (id) ON DELETE CASCADE,
	event TEXT NOT NULL,
	payload TEXT NOT NULL,
	created_at TEXT NOT NULL,
	attempts INTEGER NOT NULL DEFAULT 0,
	next_attempt_at TEXT NOT NULL,
	delivered_at TEXT,
	failed_at TEXT,
	last_error TEXT
);
INSERT INTO webhook_deliveries_old (id, webhook_id, event, payload, created_at, attempts, next_attempt_at, delivered_at, failed_at, last_error)
SELECT id, webhook_id, event, payload, created_at, attempts, next_attempt_at, delivered_at, failed_at, last_error FROM webhook_deliveries WHERE webhook_id IS NOT NULL;
DROP INDEX webhook_deliveries_due;
DROP TABLE webhook_deliveries;
ALTER TABLE webhook_deliveries_old RENAME TO webhook_deliveries;
CREATE INDEX webhook_deliveries_due ON webhook_deliveries (next_attempt_at) WHERE delivered_at IS NULL AND failed_at IS NULL;
DROP INDEX tg_channels_member;
DROP TABLE tg_channels;
DROP INDEX reinstatements_review;
DROP INDEX reinstatements_open;
DROP TABLE reinstatements;
DROP INDEX tracks_target;
DROP TABLE tracks;
DROP TABLE managing_gmails;
