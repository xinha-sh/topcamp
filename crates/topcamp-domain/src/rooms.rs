//! Room types, involvement levels, and room scopes.
//!
//! Evidence: MIGRATION_NOTES.md "Rooms + memberships + authorization
//! (domain trace supplement)" (`room_scope` per controller family;
//! `revise(grant_to … room-type default involvement)`), "Room-type
//! controllers + qr_code" (opens/closeds/directs families), "Cable /
//! WebSocket (§2.6)" (STI-aware GID params such as
//! `gid://topcamp/Rooms::Open/1`; rooms keyed by STI param key), the
//! schema plan (STI `type` keeps its text codes; `involvement` default
//! `'mentions'`; four involvement variants: invisible/nothing/mentions/
//! everything), and "Autocomplete/unfurl/refresh/involvement"
//! (blank involvement stores nil; unknown string → `ArgumentError` 500).

/// Single-table-inheritance room type (evidence: STI `type` with 3 names;
/// STI-aware stream names like `gid://topcamp/Rooms::Open/1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoomType {
    Open,
    Closed,
    Direct,
}

impl RoomType {
    /// Rails STI class name stored/compared in app logic.
    pub fn class_name(self) -> &'static str {
        match self {
            Self::Open => "Rooms::Open",
            Self::Closed => "Rooms::Closed",
            Self::Direct => "Rooms::Direct",
        }
    }

    /// Parse a Rails STI class name; anything else yields `None`.
    pub fn from_class_name(class_name: &str) -> Option<Self> {
        match class_name {
            "Rooms::Open" => Some(Self::Open),
            "Rooms::Closed" => Some(Self::Closed),
            "Rooms::Direct" => Some(Self::Direct),
            _ => None,
        }
    }
}

/// Membership involvement level (evidence: four variants
/// invisible/nothing/mentions/everything; column default `'mentions'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Involvement {
    Invisible,
    Nothing,
    #[default]
    Mentions,
    Everything,
}

impl Involvement {
    /// Stored string name for this level.
    pub fn name(self) -> &'static str {
        match self {
            Self::Invisible => "invisible",
            Self::Nothing => "nothing",
            Self::Mentions => "mentions",
            Self::Everything => "everything",
        }
    }

    /// All stored names, in canonical order.
    pub fn names() -> [&'static str; 4] {
        ["invisible", "nothing", "mentions", "everything"]
    }

    /// Parse a stored name. Returns `None` for unknown strings (evidence:
    /// unknown string → `ArgumentError` 500 at the boundary) and for blank
    /// input (evidence: blank — `""`/`[]`/whitespace/missing — stores nil,
    /// i.e. no involvement), so the caller can map `None` to either the
    /// 500 or the nil-store path.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "invisible" => Some(Self::Invisible),
            "nothing" => Some(Self::Nothing),
            "mentions" => Some(Self::Mentions),
            "everything" => Some(Self::Everything),
            _ => None,
        }
    }
}

/// Which room types a controller family may see (evidence: "`room_scope`
/// per controller family: `All` (rooms), `WithoutDirects`
/// (opens/closeds), `Directs` (directs). `set_room` misses (or
/// out-of-scope) → redirect to root WITH alert … (not 404)").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoomScope {
    All,
    WithoutDirects,
    Directs,
}

impl RoomScope {
    /// Whether a room of the given type is visible inside this scope.
    pub fn contains(self, room_type: RoomType) -> bool {
        match self {
            Self::All => true,
            Self::WithoutDirects => room_type != RoomType::Direct,
            Self::Directs => room_type == RoomType::Direct,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_type_class_names_are_sti_names() {
        // Evidence: STI-aware params like `gid://topcamp/Rooms::Open/1`.
        assert_eq!(RoomType::Open.class_name(), "Rooms::Open");
        assert_eq!(RoomType::Closed.class_name(), "Rooms::Closed");
        assert_eq!(RoomType::Direct.class_name(), "Rooms::Direct");
    }

    #[test]
    fn room_type_round_trips_through_class_name() {
        for room_type in [RoomType::Open, RoomType::Closed, RoomType::Direct] {
            assert_eq!(
                RoomType::from_class_name(room_type.class_name()),
                Some(room_type)
            );
        }
    }

    #[test]
    fn room_type_rejects_unknown_class_names() {
        assert_eq!(RoomType::from_class_name("Room"), None);
        assert_eq!(RoomType::from_class_name("Open"), None);
        assert_eq!(RoomType::from_class_name("rooms::open"), None);
        assert_eq!(RoomType::from_class_name(""), None);
        assert_eq!(RoomType::from_class_name("Rooms::Bogus"), None);
    }

    #[test]
    fn involvement_names_cover_all_four_variants() {
        assert_eq!(
            Involvement::names(),
            ["invisible", "nothing", "mentions", "everything"]
        );
        assert_eq!(Involvement::Invisible.name(), "invisible");
        assert_eq!(Involvement::Nothing.name(), "nothing");
        assert_eq!(Involvement::Mentions.name(), "mentions");
        assert_eq!(Involvement::Everything.name(), "everything");
    }

    #[test]
    fn involvement_default_is_mentions() {
        // Evidence: `involvement` column default `'mentions'`.
        assert_eq!(Involvement::default(), Involvement::Mentions);
    }

    #[test]
    fn involvement_round_trips_through_name() {
        for name in Involvement::names() {
            let parsed = Involvement::from_name(name);
            assert!(parsed.is_some());
            assert_eq!(parsed.unwrap().name(), name);
        }
    }

    #[test]
    fn involvement_rejects_unknown_strings() {
        // Evidence: unknown string → `ArgumentError` 500.
        assert_eq!(Involvement::from_name("bogus"), None);
        assert_eq!(Involvement::from_name("MENTIONS"), None);
        assert_eq!(Involvement::from_name("mention"), None);
    }

    #[test]
    fn involvement_blank_input_has_no_level() {
        // Evidence: blank (`""`/`[]`/whitespace/missing) stores nil.
        assert_eq!(Involvement::from_name(""), None);
        assert_eq!(Involvement::from_name("   "), None);
    }

    #[test]
    fn room_scope_visibility_matrix() {
        // `All` (rooms family) sees everything.
        for room_type in [RoomType::Open, RoomType::Closed, RoomType::Direct] {
            assert!(RoomScope::All.contains(room_type));
        }
        // `WithoutDirects` (opens/closeds families) hides directs only.
        assert!(RoomScope::WithoutDirects.contains(RoomType::Open));
        assert!(RoomScope::WithoutDirects.contains(RoomType::Closed));
        assert!(!RoomScope::WithoutDirects.contains(RoomType::Direct));
        // `Directs` (directs family) sees directs only.
        assert!(!RoomScope::Directs.contains(RoomType::Open));
        assert!(!RoomScope::Directs.contains(RoomType::Closed));
        assert!(RoomScope::Directs.contains(RoomType::Direct));
    }
}
