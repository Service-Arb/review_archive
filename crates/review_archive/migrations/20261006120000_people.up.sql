-- no-transaction
-- A person is a concierge account (`sub`), as the panel's assertion names it; what a member
-- keeps points at one. Rows keyed by email before this become people without a `sub`, one per
-- address, for a verified sign-in with that address to claim (`store::people`).
-- Outside a transaction: rebuilding `managing_gmails` must not cascade into what references it.
PRAGMA foreign_keys = OFF;
BEGIN IMMEDIATE;

CREATE TABLE people (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	sub TEXT UNIQUE,
	email TEXT NOT NULL,
	name TEXT NOT NULL,
	first_seen TEXT NOT NULL
);
INSERT INTO people (email, name, first_seen)
SELECT email, '', MIN(at) FROM (
	SELECT member_email AS email, created_at AS at FROM managing_gmails
	UNION ALL SELECT member_email, at FROM token_ledger
	UNION ALL SELECT member_email, created_at FROM tg_channels
) GROUP BY email;
CREATE INDEX people_email ON people (email);

CREATE TABLE managing_gmails_new (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	person_id INTEGER NOT NULL REFERENCES people (id),
	gmail TEXT NOT NULL,
	created_at TEXT NOT NULL,
	enabled INTEGER NOT NULL DEFAULT 1,
	UNIQUE (person_id, gmail)
);
INSERT INTO managing_gmails_new (id, person_id, gmail, created_at, enabled)
SELECT g.id, p.id, g.gmail, g.created_at, g.enabled FROM managing_gmails g JOIN people p ON p.email = g.member_email;
DROP TABLE managing_gmails;
ALTER TABLE managing_gmails_new RENAME TO managing_gmails;

CREATE TABLE token_ledger_new (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	person_id INTEGER NOT NULL REFERENCES people (id),
	at TEXT NOT NULL,
	delta INTEGER NOT NULL,
	kind TEXT NOT NULL CHECK (kind IN ('accrual', 'grant', 'purchase', 'set', 'charge')),
	run_id INTEGER REFERENCES runs (id),
	by_email TEXT,
	note TEXT,
	CHECK ((kind = 'charge') = (run_id IS NOT NULL))
);
INSERT INTO token_ledger_new (id, person_id, at, delta, kind, run_id, by_email, note)
SELECT l.id, p.id, l.at, l.delta, l.kind, l.run_id, l.by_email, l.note FROM token_ledger l JOIN people p ON p.email = l.member_email;
DROP INDEX token_ledger_member;
DROP TABLE token_ledger;
ALTER TABLE token_ledger_new RENAME TO token_ledger;
CREATE INDEX token_ledger_person ON token_ledger (person_id, kind, at);

CREATE TABLE tg_channels_new (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	person_id INTEGER NOT NULL REFERENCES people (id),
	-- NULL: every place the member tracks
	managing_gmail_id INTEGER REFERENCES managing_gmails (id) ON DELETE CASCADE,
	-- as the member pasted it: `@channel`, `-100…`, `<group>/<topic>`
	destination TEXT NOT NULL,
	-- JSON array of event names
	events TEXT NOT NULL,
	created_at TEXT NOT NULL
);
INSERT INTO tg_channels_new (id, person_id, managing_gmail_id, destination, events, created_at)
SELECT c.id, p.id, c.managing_gmail_id, c.destination, c.events, c.created_at FROM tg_channels c JOIN people p ON p.email = c.member_email;
DROP INDEX tg_channels_member;
DROP TABLE tg_channels;
ALTER TABLE tg_channels_new RENAME TO tg_channels;
CREATE INDEX tg_channels_person ON tg_channels (person_id);

PRAGMA foreign_key_check;
COMMIT;
PRAGMA foreign_keys = ON;
