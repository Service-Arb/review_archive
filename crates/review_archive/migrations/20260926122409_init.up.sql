-- Timestamps are TEXT in one fixed shape, `YYYY-MM-DDTHH:MM:SSZ` (UTC, whole seconds):
-- it sorts as it reads, and SQLite's date() takes it as is.

CREATE TABLE targets (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	label TEXT NOT NULL,
	kind TEXT NOT NULL CHECK (kind IN ('maps', 'gbp')),
	place_id TEXT NOT NULL,
	gbp_account TEXT,
	gbp_location TEXT,
	lang TEXT NOT NULL,
	interval_secs INTEGER NOT NULL CHECK (interval_secs >= 3600),
	enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
	created_at TEXT NOT NULL,
	CHECK (kind = 'maps' OR (gbp_account IS NOT NULL AND gbp_location IS NOT NULL))
);

CREATE TABLE reviews (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	target_id INTEGER NOT NULL REFERENCES targets (id),
	source_review_id TEXT NOT NULL,
	author TEXT NOT NULL,
	author_url TEXT,
	rating INTEGER CHECK (rating BETWEEN 1 AND 5),
	text TEXT,
	reply TEXT,
	photo_count INTEGER NOT NULL DEFAULT 0,
	published_raw TEXT,
	published_est TEXT,
	first_seen TEXT NOT NULL,
	last_seen TEXT NOT NULL,
	gone_at TEXT,
	content_hash TEXT NOT NULL,
	capture_pending INTEGER NOT NULL DEFAULT 1 CHECK (capture_pending IN (0, 1)),
	UNIQUE (target_id, source_review_id)
);
CREATE INDEX reviews_target_first_seen ON reviews (target_id, first_seen);

-- Append-only: a row per distinct content, the first sighting included.
CREATE TABLE review_versions (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	review_id INTEGER NOT NULL REFERENCES reviews (id),
	seen_at TEXT NOT NULL,
	content_hash TEXT NOT NULL,
	rating INTEGER,
	text TEXT,
	reply TEXT
);
CREATE INDEX review_versions_review ON review_versions (review_id, seen_at);

CREATE TABLE captures (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	review_id INTEGER NOT NULL REFERENCES reviews (id),
	captured_at TEXT NOT NULL,
	sha256 TEXT NOT NULL,
	width INTEGER NOT NULL,
	height INTEGER NOT NULL,
	page_url TEXT NOT NULL,
	scanner_version TEXT NOT NULL
);
CREATE INDEX captures_review ON captures (review_id);
CREATE INDEX captures_sha256 ON captures (sha256);

CREATE TABLE runs (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	target_id INTEGER NOT NULL REFERENCES targets (id),
	started_at TEXT NOT NULL,
	finished_at TEXT,
	status TEXT CHECK (status IN ('ok', 'partial', 'failed')),
	error TEXT,
	n_seen INTEGER NOT NULL DEFAULT 0,
	n_new INTEGER NOT NULL DEFAULT 0,
	n_changed INTEGER NOT NULL DEFAULT 0,
	n_gone INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX runs_target_started ON runs (target_id, started_at);
