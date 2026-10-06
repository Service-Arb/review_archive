-- no-transaction
-- Back to email keys; a person's rows go to the address they last signed in with.
PRAGMA foreign_keys = OFF;
BEGIN IMMEDIATE;

CREATE TABLE managing_gmails_old (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	member_email TEXT NOT NULL,
	gmail TEXT NOT NULL,
	created_at TEXT NOT NULL,
	enabled INTEGER NOT NULL DEFAULT 1,
	UNIQUE (member_email, gmail)
);
INSERT INTO managing_gmails_old (id, member_email, gmail, created_at, enabled)
SELECT g.id, lower(p.email), g.gmail, g.created_at, g.enabled FROM managing_gmails g JOIN people p ON p.id = g.person_id;
DROP TABLE managing_gmails;
ALTER TABLE managing_gmails_old RENAME TO managing_gmails;

CREATE TABLE token_ledger_old (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	member_email TEXT NOT NULL,
	at TEXT NOT NULL,
	delta INTEGER NOT NULL,
	kind TEXT NOT NULL CHECK (kind IN ('accrual', 'grant', 'purchase', 'set', 'charge')),
	run_id INTEGER REFERENCES runs (id),
	by_email TEXT,
	note TEXT,
	CHECK ((kind = 'charge') = (run_id IS NOT NULL))
);
INSERT INTO token_ledger_old (id, member_email, at, delta, kind, run_id, by_email, note)
SELECT l.id, lower(p.email), l.at, l.delta, l.kind, l.run_id, l.by_email, l.note FROM token_ledger l JOIN people p ON p.id = l.person_id;
DROP INDEX token_ledger_person;
DROP TABLE token_ledger;
ALTER TABLE token_ledger_old RENAME TO token_ledger;
CREATE INDEX token_ledger_member ON token_ledger (member_email, kind, at);

CREATE TABLE tg_channels_old (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	member_email TEXT NOT NULL,
	managing_gmail_id INTEGER REFERENCES managing_gmails (id) ON DELETE CASCADE,
	destination TEXT NOT NULL,
	events TEXT NOT NULL,
	created_at TEXT NOT NULL
);
INSERT INTO tg_channels_old (id, member_email, managing_gmail_id, destination, events, created_at)
SELECT c.id, lower(p.email), c.managing_gmail_id, c.destination, c.events, c.created_at FROM tg_channels c JOIN people p ON p.id = c.person_id;
DROP INDEX tg_channels_person;
DROP TABLE tg_channels;
ALTER TABLE tg_channels_old RENAME TO tg_channels;
CREATE INDEX tg_channels_member ON tg_channels (member_email);

DROP TABLE people;

PRAGMA foreign_key_check;
COMMIT;
PRAGMA foreign_keys = ON;
