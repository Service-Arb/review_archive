-- Owners' posts, as the place's overview shows the latest: one row per distinct text.
CREATE TABLE posts (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	target_id INTEGER NOT NULL REFERENCES targets (id),
	content_hash TEXT NOT NULL,
	text TEXT NOT NULL,
	published_raw TEXT,
	published_est TEXT,
	first_seen TEXT NOT NULL,
	last_seen TEXT NOT NULL,
	UNIQUE (target_id, content_hash)
);
