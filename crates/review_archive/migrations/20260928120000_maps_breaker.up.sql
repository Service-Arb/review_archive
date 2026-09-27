-- Additive only: a new table, so the previous release keeps working on this schema.

-- Google flagged the address we scan from: every Maps walk pauses until probe_after, then
-- one probes. At most one row; none while Maps is open.
CREATE TABLE maps_breaker (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    tripped_at TEXT NOT NULL,
    reason_code TEXT NOT NULL,
    trips INTEGER NOT NULL CHECK (trips > 0),
    probe_after TEXT NOT NULL
);
