-- Additive only: new tables, nothing existing changes, so the previous release keeps
-- working on this schema and rolling the image back needs no down migration.

-- On-demand work for the one browser: a scan of a target now, or an ad-hoc capture.
-- Scheduled scans do not queue here; the worker runs these ahead of them.
CREATE TABLE jobs (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	kind TEXT NOT NULL CHECK (kind IN ('scan', 'capture')),
	target_id INTEGER NOT NULL REFERENCES targets (id),
	-- capture limits as JSON: {"max_reviews": n, "review_ids": [...]}; NULL for a scan
	params TEXT,
	status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'done', 'failed')),
	created_at TEXT NOT NULL,
	started_at TEXT,
	finished_at TEXT,
	run_id INTEGER REFERENCES runs (id),
	error TEXT,
	-- the reviews the job's run listed, as a JSON array of review ids
	review_ids TEXT
);
CREATE INDEX jobs_queued ON jobs (status, id);

CREATE TABLE webhooks (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	url TEXT NOT NULL,
	-- JSON array of event names
	events TEXT NOT NULL,
	-- the HMAC key; it has to be kept to sign with
	secret TEXT NOT NULL,
	created_at TEXT NOT NULL
);

-- The outbox: a row per event per subscribed hook, written in the same transaction as
-- what caused it, so a restart loses nothing. Removing a hook drops what it still owed.
CREATE TABLE webhook_deliveries (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	webhook_id INTEGER NOT NULL REFERENCES webhooks (id) ON DELETE CASCADE,
	event TEXT NOT NULL,
	payload TEXT NOT NULL,
	created_at TEXT NOT NULL,
	attempts INTEGER NOT NULL DEFAULT 0,
	next_attempt_at TEXT NOT NULL,
	delivered_at TEXT,
	-- set when retries ran out
	failed_at TEXT,
	last_error TEXT
);
CREATE INDEX webhook_deliveries_due ON webhook_deliveries (next_attempt_at) WHERE delivered_at IS NULL AND failed_at IS NULL;
