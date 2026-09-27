-- Forgets where walks were cut short (the next scans stop at the first screen of archived
-- cards again) and which runs were ad-hoc captures (they count for the schedule again).
ALTER TABLE runs DROP COLUMN ad_hoc;
ALTER TABLE targets DROP COLUMN cut_after;
