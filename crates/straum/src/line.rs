//! How `straum shell` splits the parameters of a line, the text past its
//! semicolon, into words. `shell.rs` sends the words as an ask's
//! parameters; `complete.rs` counts them to tell which parameter comes
//! next, and asks whether a quote is still open, which means the user is
//! still typing a value. A word in single quotes may hold spaces.

/// The parameters of a shell line, the text past its semicolon, a word
/// each. A word that opens with a single quote runs to the quote that
/// closes it, spaces and all: the next `'` with whitespace or the end
/// after it. One that never closes is read to the next whitespace, as any
/// other word is, so `'` and `it's` mean what they mean to `straum ask`.
pub(crate) fn parameter_words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let length = match quoted_length(rest) {
            Some(length) => length,
            None => plain_length(rest),
        };
        words.push(&rest[..length]);
        rest = rest[length..].trim_start();
    }
    words
}

/// Whether `text` opens a quote it doesn't close: the user is still
/// typing a value. No later quote can close it, or it would be closed.
pub(crate) fn has_open_quote(text: &str) -> bool {
    for word in parameter_words(text) {
        if word.starts_with('\'') && quoted_length(word).is_none() {
            return true;
        }
    }
    false
}

/// The bytes of the quoted word `text` starts with, quotes included, if
/// it starts with one that closes.
fn quoted_length(text: &str) -> Option<usize> {
    let inside = text.strip_prefix('\'')?;
    for (at, character) in inside.char_indices() {
        if character != '\'' {
            continue;
        }
        let after = &inside[at + 1..];
        if after.is_empty() || after.starts_with(char::is_whitespace) {
            // The opening quote, what it holds and the closing quote.
            return Some(at + 2);
        }
    }
    None
}

/// The bytes of `text` before its first whitespace.
fn plain_length(text: &str) -> usize {
    match text.find(char::is_whitespace) {
        Some(at) => at,
        None => text.len(),
    }
}

#[cfg(test)]
mod tests;
