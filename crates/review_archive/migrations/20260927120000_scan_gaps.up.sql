-- Additive only: two columns with defaults, so the previous release keeps working on this
-- schema and rolling the image back needs no down migration.

-- The last card a walk read before its limit (or the page) cut it short, while no scan has
-- walked past it since: the next scan reads on below it instead of stopping at the first
-- screen of archived cards, which would leave the rest unread for good.
ALTER TABLE targets ADD COLUMN cut_after TEXT;

-- Runs of ad-hoc captures: they neither move a target's schedule nor end its first,
-- whole-list walk.
ALTER TABLE runs ADD COLUMN ad_hoc INTEGER NOT NULL DEFAULT 0 CHECK (ad_hoc IN (0, 1));
