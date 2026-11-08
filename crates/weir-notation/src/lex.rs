use std::fmt;
use std::iter::Peekable;
use std::str::Chars;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tok {
    /// A run of `[A-Za-z0-9_]` that is not all digits, e.g. `fare` or `15m`.
    Word(String),
    Int(i64),
    /// `'...'`, with `''` for a quote.
    Text(String),
    Sym(&'static str),
}

/// Splits one line into tokens, stopping at `#`.
pub(crate) fn lex(line: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '#' => break,
            c if c.is_whitespace() => {
                chars.next();
            }
            '\'' => {
                chars.next();
                toks.push(Tok::Text(text(&mut chars)?));
            }
            '-' => {
                chars.next();
                if chars.next_if_eq(&'>').is_some() {
                    toks.push(Tok::Sym("->"));
                } else {
                    let digits = word(&mut chars);
                    let n = format!("-{digits}")
                        .parse()
                        .map_err(|_| format!("bad number -{digits}"))?;
                    toks.push(Tok::Int(n));
                }
            }
            '=' | '.' | ',' | '(' | ')' | '/' => {
                chars.next();
                toks.push(Tok::Sym(match c {
                    '=' => "=",
                    '.' => ".",
                    ',' => ",",
                    '(' => "(",
                    ')' => ")",
                    _ => "/",
                }));
            }
            c if c.is_ascii_alphanumeric() || c == '_' => {
                let w = word(&mut chars);
                if w.bytes().all(|b| b.is_ascii_digit()) {
                    toks.push(Tok::Int(
                        w.parse().map_err(|_| format!("number {w} too large"))?,
                    ));
                } else {
                    toks.push(Tok::Word(w));
                }
            }
            c => return Err(format!("unexpected {c:?}")),
        }
    }
    Ok(toks)
}

fn word(chars: &mut Peekable<Chars<'_>>) -> String {
    let mut w = String::new();
    while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
        w.push(c);
    }
    w
}

fn text(chars: &mut Peekable<Chars<'_>>) -> Result<String, String> {
    let mut s = String::new();
    loop {
        match chars.next() {
            Some('\'') if chars.next_if_eq(&'\'').is_some() => s.push('\''),
            Some('\'') => return Ok(s),
            Some(c) => s.push(c),
            None => return Err("unterminated text".into()),
        }
    }
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Word(w) => f.write_str(w),
            Tok::Int(n) => write!(f, "{n}"),
            Tok::Text(s) => write!(f, "'{}'", s.replace('\'', "''")),
            Tok::Sym(s) => f.write_str(s),
        }
    }
}
