//! Cuts a spec's text into its statements, as written, so a reader can see
//! the `CREATE VIEW` that declared a view. `sqlparser` splits the text
//! into tokens; a statement runs from its first token that is not blank
//! to its semicolon, or to the end of the text for the last. The comments
//! right above it are its own; a blank line ends them, so a file's header
//! stays out. A part holding only comments is no statement, as the parser
//! skips it too, so the parts line up with the views.

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

use super::Offsets;

/// Each statement of `text` in order, as written; none for a text the
/// tokenizer refuses, which a spec that parsed never is.
pub(crate) fn statement_texts(text: &str) -> Vec<String> {
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, text).tokenize_with_location() else {
        return Vec::new();
    };
    let offsets = Offsets::of(text);
    let mut statements = Vec::new();
    let mut start: Option<usize> = None;
    let mut has_code = false;
    // Line ends since the last token that is not blank: two make a blank
    // line.
    let mut line_ends = 0;
    for token in tokens {
        match token.token {
            Token::Whitespace(Whitespace::Space | Whitespace::Tab) | Token::EOF => {}
            Token::Whitespace(Whitespace::Newline) => {
                line_ends += 1;
                if line_ends >= 2 && !has_code {
                    start = None;
                }
            }
            Token::SemiColon => {
                let first = start.take();
                if has_code && let Some(first) = first {
                    let end = offsets.offset(token.span.end);
                    statements.push(text[first..end].to_owned());
                }
                has_code = false;
                line_ends = 0;
            }
            Token::Whitespace(_) => {
                start.get_or_insert(offsets.offset(token.span.start));
                line_ends = 0;
            }
            _ => {
                start.get_or_insert(offsets.offset(token.span.start));
                has_code = true;
                line_ends = 0;
            }
        }
    }
    if has_code && let Some(first) = start {
        statements.push(text[first..].trim_end().to_owned());
    }
    statements
}
