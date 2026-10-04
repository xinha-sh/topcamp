-- 0006_boost_content_text.sql: bot boost contents are unbounded.
--
-- Upstream declares `boosts.content` as `varchar(16)` but runs on
-- SQLite, which ignores the length: any content stores fine. Postgres
-- enforces it, so long bot boosts would 500 here while succeeding
-- there. TEXT matches the observable behavior.

ALTER TABLE boosts ALTER COLUMN content TYPE TEXT;
