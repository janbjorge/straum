use std::fmt;

/// Longest identifier PostgreSQL keeps (`NAMEDATALEN - 1`).
const MAX_LEN: usize = 63;

/// An unquoted PostgreSQL identifier: `[a-z_][a-z0-9_]*`, at most 63 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ident(String);

impl Ident {
    pub fn new(s: &str) -> Option<Ident> {
        let mut chars = s.chars();
        let first = chars.next()?;
        let ok = s.len() <= MAX_LEN
            && (first.is_ascii_lowercase() || first == '_')
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        ok.then(|| Ident(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Ident;

    #[test]
    fn unquoted_postgres_identifiers_only() {
        assert_eq!(Ident::new("_a1").unwrap().as_str(), "_a1");
        assert!(Ident::new(&"a".repeat(63)).is_some());
        let long = "a".repeat(64);
        for bad in ["", "1a", "A", "a-b", "é", long.as_str()] {
            assert_eq!(Ident::new(bad), None, "{bad}");
        }
    }
}
