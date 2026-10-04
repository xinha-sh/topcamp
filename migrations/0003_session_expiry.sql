-- 0003_session_expiry.sql: expiry for Topcoat-owned session tokens (§9).
--
-- Token issuance/cookies are owned by Topcoat (`topcoat-session`); the app
-- persists only the SHA-256 token hash (hex, in `sessions.token`) with the
-- expiry Topcoat assigned. `expires_at` is enforced in the repository
-- queries (`find_by_token` rejects expired rows); `last_active_at` keeps its
-- activity meaning for the hourly resume schedule.

ALTER TABLE sessions ADD COLUMN expires_at TIMESTAMPTZ NOT NULL DEFAULT now();
