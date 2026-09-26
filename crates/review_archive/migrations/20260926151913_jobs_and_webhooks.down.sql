-- Drops the job history, the hooks and any undelivered events with them; the archive
-- itself (targets, reviews, captures, runs) is untouched.
DROP INDEX webhook_deliveries_due;
DROP TABLE webhook_deliveries;
DROP TABLE webhooks;
DROP INDEX jobs_queued;
DROP TABLE jobs;
