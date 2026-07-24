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

#[cfg(test)]
mod tests {
    use super::LibraryId;

    #[test]
    fn library_ids_are_positive() {
        assert_eq!(LibraryId::new(1).map(LibraryId::get), Some(1));
        assert_eq!(LibraryId::new(0), None);
        assert_eq!(LibraryId::new(-1), None);
    }
}
