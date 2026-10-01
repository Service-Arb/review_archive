-- Additive only: new columns with defaults, so the previous release keeps working on this schema.

-- A member's own switches: a target is scanned while any track of it is on under a gmail that is on.
ALTER TABLE managing_gmails ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE tracks ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
