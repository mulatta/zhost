//! Strong identifiers shared by request resolution and persistence.

/// Database identifier for one personal or group library.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LibraryId(i64);

impl LibraryId {
    pub fn new(value: i64) -> Option<Self> {
        (value > 0).then_some(Self(value))
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// Database identifier for one local user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserId(i64);

impl UserId {
    pub fn new(value: i64) -> Option<Self> {
        (value > 0).then_some(Self(value))
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// Durable identity and personal-library ownership resolved for one request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    pub user_id: UserId,
    pub username: String,
    pub display_name: String,
    pub library_id: LibraryId,
}

/// Personal-library capabilities carried by an API key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions {
    pub library: bool,
    pub notes: bool,
    pub write: bool,
    pub files: bool,
}

impl Permissions {
    /// Static keys remain a recovery path with the legacy broad read scope.
    pub fn recovery(write: bool) -> Self {
        Self {
            library: true,
            notes: true,
            write,
            files: true,
        }
    }
}

/// Authentication result inserted into Axum request extensions.
///
/// Deliberately lacks `Debug`: `presented_key` is the raw secret that Zotero
/// expects `/keys/current` to echo, and must never enter diagnostic output.
#[derive(Clone)]
pub struct RequestContext {
    pub presented_key: String,
    pub principal: Principal,
    pub permissions: Permissions,
}

#[cfg(test)]
mod tests {
    use super::{LibraryId, Permissions, UserId};

    #[test]
    fn database_ids_are_positive() {
        assert_eq!(LibraryId::new(1).map(LibraryId::get), Some(1));
        assert_eq!(LibraryId::new(0), None);
        assert_eq!(LibraryId::new(-1), None);
        assert_eq!(UserId::new(1).map(UserId::get), Some(1));
        assert_eq!(UserId::new(0), None);
        assert_eq!(UserId::new(-1), None);
    }

    #[test]
    fn recovery_keys_preserve_legacy_read_scope() {
        assert_eq!(
            Permissions::recovery(false),
            Permissions {
                library: true,
                notes: true,
                write: false,
                files: true,
            }
        );
    }
}
