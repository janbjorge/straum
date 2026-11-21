//! The tests of `line.rs`: a shell line's parameters split into words,
//! and a quote told open or closed.

use super::*;

#[test]
fn a_quoted_parameter_word_holds_its_spaces() {
    let cases: [(&str, &[&str]); 7] = [
        (" 7  paid\ttrue ", &["7", "paid", "true"]),
        ("'Staten Island' 7", &["'Staten Island'", "7"]),
        ("'O'Brien  St' x", &["'O'Brien  St'", "x"]),
        ("'' 'blå bær'", &["''", "'blå bær'"]),
        ("'7' '", &["'7'", "'"]),
        ("it's 'a", &["it's", "'a"]),
        ("", &[]),
    ];
    for (text, words) in cases {
        assert_eq!(parameter_words(text), words, "{text}");
    }
}

#[test]
fn a_quote_that_never_closes_is_a_word_like_any_other() {
    assert_eq!(parameter_words("'Staten Island"), ["'Staten", "Island"]);
    assert_eq!(parameter_words("'a'b c"), ["'a'b", "c"]);
    for open in ["'", "x 'Staten", "'Staten Is", "'a'b"] {
        assert!(has_open_quote(open), "{open}");
    }
    for closed in ["", "x", "it's", "'Staten Island'", "'' y", "'a' 'b'"] {
        assert!(!has_open_quote(closed), "{closed}");
    }
}
