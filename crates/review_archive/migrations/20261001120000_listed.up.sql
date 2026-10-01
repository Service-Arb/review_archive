-- Additive only: a new nullable column, so the previous release keeps working on this schema.

-- How many reviews the source last said the place has (Maps' count, GBP's totalReviewCount).
ALTER TABLE targets ADD COLUMN listed INTEGER;
