-- New tables, and the outbox rebuilt to take a Telegram channel in place of a hook. Its
-- existing rows and columns stay as they were, so the previous release keeps working on
-- this schema (it joins deliveries to hooks, which leaves Telegram's out).

-- A member is the email playbook's introspection answers with; membership is playbook's.
-- A managing gmail groups the places a member manages under one Google manager account.
CREATE TABLE managing_gmails (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	member_email TEXT NOT NULL,
	gmail TEXT NOT NULL,
	created_at TEXT NOT NULL,
	UNIQUE (member_email, gmail)
);

-- Targets stay shared: two members on one place cost one scan.
CREATE TABLE tracks (
	managing_gmail_id INTEGER NOT NULL REFERENCES managing_gmails (id) ON DELETE CASCADE,
	target_id INTEGER NOT NULL REFERENCES targets (id),
	created_at TEXT NOT NULL,
	PRIMARY KEY (managing_gmail_id, target_id)
);
CREATE INDEX tracks_target ON tracks (target_id);

-- Appeals of removed reviews: withdrawn, never deleted. One is open until it is withdrawn
-- or a scan lists the review again (`reinstated_at`, set in that scan's transaction).
CREATE TABLE reinstatements (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	managing_gmail_id INTEGER NOT NULL REFERENCES managing_gmails (id),
	review_id INTEGER NOT NULL REFERENCES reviews (id),
	requested_at TEXT NOT NULL,
	withdrawn_at TEXT,
	reinstated_at TEXT
);
CREATE UNIQUE INDEX reinstatements_open ON reinstatements (managing_gmail_id, review_id) WHERE withdrawn_at IS NULL AND reinstated_at IS NULL;
CREATE INDEX reinstatements_review ON reinstatements (review_id);

CREATE TABLE tg_channels (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	member_email TEXT NOT NULL,
	-- NULL: every place the member tracks
	managing_gmail_id INTEGER REFERENCES managing_gmails (id) ON DELETE CASCADE,
	-- as the member pasted it: `@channel`, `-100…`, `<group>/<topic>`
	destination TEXT NOT NULL,
	-- JSON array of event names
	events TEXT NOT NULL,
	created_at TEXT NOT NULL
);
CREATE INDEX tg_channels_member ON tg_channels (member_email);

CREATE TABLE webhook_deliveries_new (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	webhook_id INTEGER REFERENCES webhooks (id) ON DELETE CASCADE,
	tg_channel_id INTEGER REFERENCES tg_channels (id) ON DELETE CASCADE,
	event TEXT NOT NULL,
	payload TEXT NOT NULL,
	created_at TEXT NOT NULL,
	attempts INTEGER NOT NULL DEFAULT 0,
	next_attempt_at TEXT NOT NULL,
	delivered_at TEXT,
	failed_at TEXT,
	last_error TEXT,
	CHECK ((webhook_id IS NULL) <> (tg_channel_id IS NULL))
);
INSERT INTO webhook_deliveries_new (id, webhook_id, event, payload, created_at, attempts, next_attempt_at, delivered_at, failed_at, last_error)
SELECT id, webhook_id, event, payload, created_at, attempts, next_attempt_at, delivered_at, failed_at, last_error FROM webhook_deliveries;
DROP INDEX webhook_deliveries_due;
DROP TABLE webhook_deliveries;
ALTER TABLE webhook_deliveries_new RENAME TO webhook_deliveries;
CREATE INDEX webhook_deliveries_due ON webhook_deliveries (next_attempt_at) WHERE delivered_at IS NULL AND failed_at IS NULL;
