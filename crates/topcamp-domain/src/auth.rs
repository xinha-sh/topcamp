//! User roles, statuses, authorization, and session-resume rules.
//!
//! Evidence: MIGRATION_NOTES.md "Rooms + memberships + authorization
//! (domain trace supplement)" (`User::can_administer`: administrator OR
//! record creator OR new-record; 403 otherwise), "Auth / sessions (§2.4)"
//! (`resume_session` refreshes at most hourly via `ACTIVITY_REFRESH_RATE`;
//! cookie re-signed on the same schedule), and the PostgreSQL schema plan
//! (role/status kept as integer codes compared in app logic).

/// Integer-coded user role (see schema plan: codes are behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum UserRole {
    Member = 0,
    Administrator = 1,
    Bot = 2,
}

impl UserRole {
    /// Database integer code for this role.
    pub fn value(self) -> i32 {
        self as i32
    }

    /// Parse a database integer code; unknown codes yield `None`.
    pub fn from_value(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Member),
            1 => Some(Self::Administrator),
            2 => Some(Self::Bot),
            _ => None,
        }
    }

    /// Role changes allowlist to member/administrator only (evidence:
    /// "role change allowlists to member/administrator (anything else →
    /// member)"); anything else falls back to `Member`.
    pub fn from_value_or_member(value: i32) -> Self {
        match Self::from_value(value) {
            Some(Self::Member) | Some(Self::Administrator) => {
                // Unwrap is safe: just matched these two variants.
                Self::from_value(value).unwrap_or(Self::Member)
            }
            _ => Self::Member,
        }
    }

    pub fn is_administrator(self) -> bool {
        self == Self::Administrator
    }
}

/// Integer-coded user status (active / deactivated via destroy-as-deactivate /
/// banned via admin ban; evidence: "destroy = deactivate", "bans —
/// admin-only ban/unban").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum UserStatus {
    Active = 0,
    Deactivated = 1,
    Banned = 2,
}

impl UserStatus {
    /// Database integer code for this status.
    pub fn value(self) -> i32 {
        self as i32
    }

    /// Parse a database integer code; unknown codes yield `None`.
    pub fn from_value(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Active),
            1 => Some(Self::Deactivated),
            2 => Some(Self::Banned),
            _ => None,
        }
    }

    pub fn is_active(self) -> bool {
        self == Self::Active
    }
}

/// Single authorization predicate `User::can_administer` (evidence:
/// "administrator OR record creator OR new-record — used for rooms AND
/// messages (`ensure_can_administer`, 403 otherwise)").
pub fn can_administer(is_admin: bool, is_creator: bool, is_new: bool) -> bool {
    is_admin || is_creator || is_new
}

/// Minimum age of `last_active_at` before a session takes the DB writer
/// again (evidence: `ACTIVITY_REFRESH_RATE`, hourly refresh of
/// `last_active_at`/UA/IP; "only due sessions take the DB writer; the
/// cookie is re-signed on the same schedule, not per request").
pub const ACTIVITY_REFRESH_INTERVAL_SECS: i64 = 3_600;

/// Whether a session with the given `last_active_at` (unix seconds) is due
/// for a refresh at `now` (unix seconds). True once the stored timestamp is
/// at least [`ACTIVITY_REFRESH_INTERVAL_SECS`] old; clock skew (`now` before
/// `last_active_at`) is never due.
pub fn session_resume_due(last_active_at_unix: i64, now_unix: i64) -> bool {
    now_unix.saturating_sub(last_active_at_unix) >= ACTIVITY_REFRESH_INTERVAL_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_codes_match_schema() {
        assert_eq!(UserRole::Member.value(), 0);
        assert_eq!(UserRole::Administrator.value(), 1);
        assert_eq!(UserRole::Bot.value(), 2);
    }

    #[test]
    fn role_round_trips() {
        for role in [UserRole::Member, UserRole::Administrator, UserRole::Bot] {
            assert_eq!(UserRole::from_value(role.value()), Some(role));
        }
    }

    #[test]
    fn role_rejects_unknown_codes() {
        assert_eq!(UserRole::from_value(-1), None);
        assert_eq!(UserRole::from_value(3), None);
        assert_eq!(UserRole::from_value(99), None);
    }

    #[test]
    fn role_change_allowlist_falls_back_to_member() {
        // Evidence: role change allowlists to member/administrator
        // (anything else → member).
        assert_eq!(UserRole::from_value_or_member(0), UserRole::Member);
        assert_eq!(UserRole::from_value_or_member(1), UserRole::Administrator);
        assert_eq!(UserRole::from_value_or_member(2), UserRole::Member);
        assert_eq!(UserRole::from_value_or_member(99), UserRole::Member);
    }

    #[test]
    fn status_codes_match_schema() {
        assert_eq!(UserStatus::Active.value(), 0);
        assert_eq!(UserStatus::Deactivated.value(), 1);
        assert_eq!(UserStatus::Banned.value(), 2);
    }

    #[test]
    fn status_round_trips() {
        for status in [
            UserStatus::Active,
            UserStatus::Deactivated,
            UserStatus::Banned,
        ] {
            assert_eq!(UserStatus::from_value(status.value()), Some(status));
        }
        assert_eq!(UserStatus::from_value(7), None);
    }

    #[test]
    fn only_active_status_is_active() {
        assert!(UserStatus::Active.is_active());
        assert!(!UserStatus::Deactivated.is_active());
        assert!(!UserStatus::Banned.is_active());
    }

    #[test]
    fn can_administer_truth_table() {
        // administrator OR creator OR new-record.
        assert!(!can_administer(false, false, false));
        assert!(can_administer(true, false, false));
        assert!(can_administer(false, true, false));
        assert!(can_administer(false, false, true));
        assert!(can_administer(true, true, true));
        assert!(can_administer(true, false, true));
        assert!(can_administer(false, true, true));
    }

    #[test]
    fn resume_interval_is_hourly() {
        assert_eq!(ACTIVITY_REFRESH_INTERVAL_SECS, 3_600);
    }

    #[test]
    fn session_resume_due_only_when_hour_old() {
        let now = 1_000_000;
        // Fresh session: no writer, no cookie re-sign.
        assert!(!session_resume_due(now, now));
        assert!(!session_resume_due(now - 3_599, now));
        // Due at exactly the hourly boundary and beyond.
        assert!(session_resume_due(now - 3_600, now));
        assert!(session_resume_due(now - 7_200, now));
        assert!(session_resume_due(0, now));
    }

    #[test]
    fn session_resume_due_tolerates_clock_skew() {
        // Stored timestamp in the future must not trigger a refresh.
        assert!(!session_resume_due(1_000_100, 1_000_000));
    }
}
