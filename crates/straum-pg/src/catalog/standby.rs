//! Whether PostgreSQL's `synchronous_standby_names` could count Straum
//! as a synchronous standby. The setting lists names, bare or in double
//! quotes, alone or inside `FIRST n (..)`, `ANY n (..)` or `n (..)`; `*`
//! names any standby. The setting is split into those names, and each is
//! compared with Straum's `application_name`.

use std::iter::Peekable;
use std::str::Chars;

/// The name that stands for any standby.
const ANY_STANDBY: &str = "*";

/// Whether `setting` names `application_name`, or any standby.
/// PostgreSQL compares the two without regard to ASCII case, quoted or
/// not (`SyncRepGetStandbyPriority` in `syncrep.c`).
pub(super) fn names_standby(setting: &str, application_name: &str) -> bool {
    for name in standby_names(setting) {
        if name == ANY_STANDBY || name.eq_ignore_ascii_case(application_name) {
            return true;
        }
    }
    false
}

/// The names `setting` lists: those in its parentheses when it has them,
/// whose words before are a method and a count, else every one.
fn standby_names(setting: &str) -> Vec<String> {
    let mut outside = Vec::new();
    let mut inside = Vec::new();
    let mut has_parentheses = false;
    let mut in_parentheses = false;
    let mut characters = setting.chars().peekable();
    while let Some(&character) = characters.peek() {
        let name = match character {
            '(' | ')' => {
                has_parentheses = true;
                in_parentheses = character == '(';
                characters.next();
                continue;
            }
            ',' => {
                characters.next();
                continue;
            }
            '"' => quoted_name(&mut characters),
            _ if character.is_whitespace() => {
                characters.next();
                continue;
            }
            _ => bare_name(&mut characters),
        };
        if in_parentheses {
            inside.push(name);
        } else {
            outside.push(name);
        }
    }
    if has_parentheses { inside } else { outside }
}

/// A name in double quotes, `""` standing for one; `characters` starts
/// at the opening quote.
fn quoted_name(characters: &mut Peekable<Chars<'_>>) -> String {
    characters.next();
    let mut name = String::new();
    while let Some(character) = characters.next() {
        if character != '"' {
            name.push(character);
            continue;
        }
        if characters.peek() != Some(&'"') {
            break;
        }
        characters.next();
        name.push('"');
    }
    name
}

/// A name without quotes: up to a space, comma, parenthesis or quote.
fn bare_name(characters: &mut Peekable<Chars<'_>>) -> String {
    let mut name = String::new();
    while let Some(&character) = characters.peek() {
        if character.is_whitespace() || matches!(character, ',' | '(' | ')' | '"') {
            break;
        }
        name.push(character);
        characters.next();
    }
    name
}

#[cfg(test)]
mod tests {
    use super::names_standby;

    #[test]
    fn a_setting_that_could_name_straum() {
        for setting in [
            "*",
            "straum",
            "STRAUM",
            "a, straum",
            "a,straum",
            "\"straum\"",
            "\"Straum\"",
            "FIRST 1 (a, straum)",
            "first 2(a,*)",
            "ANY 1 (*)",
            "any 1 (a, \"straum\")",
            "2 (straum, b)",
            "a, *",
        ] {
            assert!(names_standby(setting, "straum"), "{setting}");
        }
    }

    #[test]
    fn a_setting_that_names_other_standbys() {
        for setting in [
            "",
            "  ",
            "a",
            "a, b",
            "straum_replica",
            "\"straum, b\"",
            "\"(straum)\"",
            "\"a\"\"straum\"",
            "FIRST 1 (a, b)",
            "ANY 2 (a, b, c)",
            "1 (a)",
            // A name that reads as a method's word is a name in a plain
            // list, and the method's word is no name.
            "first, any",
            "ANY 1 (first)",
        ] {
            assert!(!names_standby(setting, "straum"), "{setting}");
        }
    }
}
