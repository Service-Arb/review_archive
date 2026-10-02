-- Risk tokens: a member's balance is the sum of their rows; rows are never changed or deleted.
CREATE TABLE token_ledger (
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
CREATE INDEX token_ledger_member ON token_ledger (member_email, kind, at);

-- What the run's walk cost, paid or not.
ALTER TABLE runs ADD COLUMN tokens INTEGER NOT NULL DEFAULT 0;
