//! The names a spec and an ask use: a view, a key, a measure, a column.
//! Straum SQL reads each into an [`Identifier`], which only an unquoted
//! lowercase PostgreSQL name can become, so a name never needs quoting.

use std::fmt;

/// Longest identifier PostgreSQL keeps (`NAMEDATALEN - 1`).
const MAX_BYTES: usize = 63;

/// An unquoted PostgreSQL identifier: `[a-z_][a-z0-9_]*`, at most 63 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Identifier(String);

impl Identifier {
    /// The identifier `text` spells, if it is one.
    pub fn new(text: &str) -> Option<Identifier> {
        if text.len() > MAX_BYTES {
            return None;
        }
        let mut characters = text.chars();
        let first = characters.next()?;
        if !(first.is_ascii_lowercase() || first == '_') {
            return None;
        }
        for character in characters {
            if !(character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_') {
                return None;
            }
        }
        Some(Identifier(text.to_owned()))
    }

    /// The identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Identifier;

    #[test]
    fn unquoted_postgres_identifiers_only() {
        assert_eq!(Identifier::new("_a1").unwrap().as_str(), "_a1");
        assert!(Identifier::new(&"a".repeat(63)).is_some());
        let long = "a".repeat(64);
        for bad in ["", "1a", "A", "a-b", "é", long.as_str()] {
            assert_eq!(Identifier::new(bad), None, "{bad}");
        }
    }
}
