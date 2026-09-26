-- Irreversible in substance: this drops the archive. The only way back from a
-- run of it is a restore of the data dir from backup.
DROP TABLE runs;
DROP TABLE captures;
DROP TABLE review_versions;
DROP TABLE reviews;
DROP TABLE targets;
