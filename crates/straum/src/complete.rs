//! What Tab offers in `straum shell`, and the hint shown past the cursor.
//! `read_views` reads the views a Straum serves as JSON; `complete` and
//! `hint` take those views and the line being edited and look at the words
//! before the cursor: a view after `FROM`, `\rows` or `\d`, its keys and
//! measures after `SELECT`, a whole condition after `WHERE`, the selected
//! keys after `GROUP BY`, a selected measure and a comparison after
//! `HAVING`, the bounds of the buckets held where a parameter is a bound.
//! Plain functions, with no terminal and no network: `shell.rs` calls
//! them. A line is split into words, never parsed: Straum checks the
//! ask.

use std::fmt;

use serde_json::Value;

use crate::line::{has_open_quote, parameter_words};

const COMMANDS: [&str; 4] = ["\\d", "\\rows ", "\\?", "\\q"];
/// What HAVING compares a measure with, as the ask writes it.
const COMPARISONS: [&str; 6] = ["=", "<>", "<", "<=", ">", ">="];

/// A view as `GET /views` describes it in JSON: what a line may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct View {
    name: String,
    /// The name of the view's time, which the range bounds.
    time: String,
    /// The time first.
    keys: Vec<Key>,
    measures: Vec<String>,
    /// The start of the first bucket held and the end of the last.
    from: Option<String>,
    to: Option<String>,
}

impl View {
    /// The key of this name, if the view has one.
    fn key(&self, name: &str) -> Option<&Key> {
        for key in &self.keys {
            if key.name == name {
                return Some(key);
            }
        }
        None
    }

    fn has_measure(&self, name: &str) -> bool {
        for measure in &self.measures {
            if measure == name {
                return true;
            }
        }
        false
    }
}

/// A key of a view and its PostgreSQL type, as `straum views` writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    name: String,
    column_type: String,
}

/// A `$n` in a query: which parameter it is, counted from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ParameterNumber(usize);

impl ParameterNumber {
    /// The number after this one; the highest number a line can hold
    /// has no next, so the hint gives it again rather than panic.
    fn next(self) -> ParameterNumber {
        ParameterNumber(self.0.saturating_add(1))
    }
}

impl fmt::Display for ParameterNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "${}", self.0)
    }
}

/// The views in the JSON of `GET /views`, or `None` for JSON that isn't
/// the views.
pub(crate) fn read_views(json: &str) -> Option<Vec<View>> {
    let json: Value = serde_json::from_str(json).ok()?;
    let mut views = Vec::new();
    for view in json.get("views")?.as_array()? {
        views.push(read_view(view)?);
    }
    Some(views)
}

fn read_view(view: &Value) -> Option<View> {
    let mut keys = Vec::new();
    for key in view.get("keys")?.as_array()? {
        keys.push(read_key(key)?);
    }
    let mut measures = Vec::new();
    for measure in view.get("measures")?.as_array()? {
        measures.push(read_text(measure)?);
    }
    Some(View {
        name: read_text(view.get("name")?)?,
        time: read_text(view.get("time")?)?,
        keys,
        measures,
        from: view.get("from").and_then(read_text),
        to: view.get("to").and_then(read_text),
    })
}

fn read_key(key: &Value) -> Option<Key> {
    let name = read_text(key.get("name")?)?;
    let mut column_type = read_text(key.get("type")?)?;
    if let Some(scale) = key.get("scale")
        && let Some(scale) = scale.as_u64()
    {
        column_type = format!("{column_type} scale {scale}");
    }
    Some(Key { name, column_type })
}

fn read_text(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    Some(text.to_owned())
}

/// What Tab offers: each candidate replaces the line from `start`, a byte
/// position, to the cursor.
pub(crate) struct Completion {
    pub(crate) start: usize,
    pub(crate) candidates: Vec<String>,
}

fn is_word_break(character: char) -> bool {
    character.is_whitespace() || ",()".contains(character)
}

/// What kind of word a character belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WordKind {
    /// A name, a keyword or a `$n`.
    Name,
    /// `=`, `>=` or `<`.
    Operator,
}

/// The kind of word `character` is part of, or `None` between words.
fn word_kind(character: char) -> Option<WordKind> {
    if is_word_break(character) {
        return None;
    }
    if "<>=".contains(character) {
        return Some(WordKind::Operator);
    }
    Some(WordKind::Name)
}

/// Where the word being read starts in the text, and its kind.
struct WordStart {
    at: usize,
    kind: WordKind,
}

/// The words of a query: names, keywords, `$n` and operators. Commas and
/// parentheses only part them.
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<WordStart> = None;
    for (at, character) in text.char_indices() {
        let kind = word_kind(character);
        if let Some(start) = &word
            && kind != Some(start.kind)
        {
            words.push(text[start.at..at].to_owned());
            word = None;
        }
        if word.is_none()
            && let Some(kind) = kind
        {
            word = Some(WordStart { at, kind });
        }
    }
    if let Some(start) = word {
        words.push(text[start.at..].to_owned());
    }
    words
}

fn is(word: &str, keyword: &str) -> bool {
    word.eq_ignore_ascii_case(keyword)
}

/// The words after the last `keyword`, if the words hold it.
fn after(words: &[String], keyword: &str) -> Option<Vec<String>> {
    let mut last = None;
    for (position, word) in words.iter().enumerate() {
        if is(word, keyword) {
            last = Some(position);
        }
    }
    let position = last?;
    Some(words[position + 1..].to_vec())
}

/// Where the word the cursor is in starts, as a byte position in `before`.
fn word_start(before: &str, is_break: fn(char) -> bool) -> usize {
    let mut word_bytes = 0;
    for character in before.chars().rev() {
        if is_break(character) {
            break;
        }
        word_bytes += character.len_utf8();
    }
    before.len() - word_bytes
}

/// What Tab offers with the cursor at byte `cursor` of `line`.
pub(crate) fn complete(views: &[View], line: &str, cursor: usize) -> Completion {
    let before = &line[..cursor];
    let offered = if let Some((query, _)) = before.split_once(';') {
        parameter_candidates(views, before, query)
    } else {
        let start = word_start(before, is_word_break);
        let candidates = query_candidates(views, line, &before[..start]);
        Completion { start, candidates }
    };
    let typed = &before[offered.start..];
    let mut candidates = Vec::new();
    for mut candidate in offered.candidates {
        if !starts_with_ignoring_case(&candidate, typed) {
            continue;
        }
        // A finished word ends in a space, so the next is typed straight
        // on, as readline does; `trips ,` still reads as `trips,`.
        if !candidate.ends_with(' ') {
            candidate.push(' ');
        }
        candidates.push(candidate);
    }
    Completion {
        start: offered.start,
        candidates,
    }
}

/// What Tab offers past the semicolon: the values the next parameter may
/// take.
fn parameter_candidates(views: &[View], before: &str, query: &str) -> Completion {
    let parameters_start = query.len() + 1;
    // A parameter starts no earlier than the semicolon ends.
    let start = word_start(before, char::is_whitespace).max(parameters_start);
    let given = &before[parameters_start..start];
    // Inside an open quote the word being typed is not a value yet.
    if has_open_quote(&before[parameters_start..]) {
        return Completion {
            start,
            candidates: Vec::new(),
        };
    }
    let candidates = match slot(views, query, given) {
        Some(slot) => slot.values(),
        None => Vec::new(),
    };
    Completion { start, candidates }
}

fn starts_with_ignoring_case(candidate: &str, typed: &str) -> bool {
    let Some(head) = candidate.get(..typed.len()) else {
        return false;
    };
    head.eq_ignore_ascii_case(typed)
}

/// The view the line names after `FROM`, if Straum serves one by that name.
fn view_after_from(views: &[View], words: &[String]) -> Option<View> {
    let named = after(words, "FROM")?;
    let name = named.first()?;
    for view in views {
        if view.name == *name {
            return Some(view.clone());
        }
    }
    None
}

fn names_of(view: &View) -> Vec<String> {
    let mut names = Vec::with_capacity(view.keys.len() + view.measures.len());
    for key in &view.keys {
        names.push(key.name.clone());
    }
    for measure in &view.measures {
        names.push(measure.clone());
    }
    names
}

/// What may come next in a query, after the words `before` the cursor's.
fn query_candidates(views: &[View], line: &str, before: &str) -> Vec<String> {
    let before = words(before);
    let all = words(line);
    let Some(first) = before.first() else {
        return line_starts();
    };
    let takes_a_view = is(first, "\\rows") || is(first, "\\d");
    if takes_a_view && before.len() == 1 {
        return view_names(views);
    }
    let view = view_after_from(views, &all);
    let selected = selected_keys(view.as_ref(), &all);

    if let Some(conditions) = after(&before, "HAVING") {
        return having_candidates(view.as_ref(), &all, &conditions);
    }
    if let Some(listed) = after(&before, "GROUP") {
        return group_by_candidates(&selected, &listed);
    }
    if let Some(conditions) = after(&before, "WHERE") {
        return condition_candidates(view.as_ref(), &all, &conditions, &selected);
    }
    if let Some(named) = after(&before, "FROM") {
        if named.is_empty() {
            return view_names(views);
        }
        let mut next = vec!["WHERE ".to_owned()];
        next.extend(query_ends(&selected));
        return next;
    }
    if let Some(listed) = after(&before, "SELECT") {
        return select_candidates(views, view.as_ref(), &listed);
    }
    Vec::new()
}

/// What a line may start with: a query or a shell command.
fn line_starts() -> Vec<String> {
    let mut starts = vec!["SELECT ".to_owned()];
    for command in COMMANDS {
        starts.push(command.to_owned());
    }
    starts
}

/// Each view's name.
fn view_names(views: &[View]) -> Vec<String> {
    let mut names = Vec::with_capacity(views.len());
    for view in views {
        names.push(view.name.clone());
    }
    names
}

/// What may follow `GROUP`: `BY`, then the selected keys not yet listed.
fn group_by_candidates(selected: &[String], listed: &[String]) -> Vec<String> {
    match listed {
        [] => vec!["BY ".to_owned()],
        [_by] => vec![selected.join(", ")],
        [_by, listed @ ..] => {
            let mut left = Vec::new();
            for key in selected {
                if !listed.contains(key) {
                    left.push(key.clone());
                }
            }
            if left.is_empty() {
                vec!["; ".to_owned(), "HAVING ".to_owned()]
            } else {
                left
            }
        }
    }
}

/// What may follow `SELECT`: the names not yet listed, then `FROM`.
fn select_candidates(views: &[View], view: Option<&View>, listed: &[String]) -> Vec<String> {
    let offered = if let Some(view) = view {
        names_of(view)
    } else {
        // Before FROM names the view, any view's names may be meant.
        let mut offered = Vec::new();
        for each in views {
            offered.extend(names_of(each));
        }
        offered
    };
    let mut names: Vec<String> = Vec::new();
    for name in offered {
        if !names.contains(&name) && !listed.contains(&name) {
            names.push(name);
        }
    }
    if !listed.is_empty() {
        names.push("FROM ".to_owned());
    }
    names
}

/// The keys between `SELECT` and `FROM`, which `GROUP BY` names again.
fn selected_keys(view: Option<&View>, all: &[String]) -> Vec<String> {
    let Some(view) = view else {
        return Vec::new();
    };
    let mut selected = Vec::new();
    for word in selected_words(all) {
        if view.key(&word).is_some() {
            selected.push(word);
        }
    }
    selected
}

/// The words between `SELECT` and `FROM`.
fn selected_words(all: &[String]) -> Vec<String> {
    let listed = after(all, "SELECT").unwrap_or_default();
    let mut selected = Vec::new();
    for word in listed {
        if is(&word, "FROM") {
            break;
        }
        selected.push(word);
    }
    selected
}

fn has_bound(time: &str, all: &[String], operator: &str) -> bool {
    for pair in all.windows(2) {
        if pair[0] == time && pair[1] == operator {
            return true;
        }
    }
    false
}

/// Which bounds of the range the query has written: `<time> >=` and
/// `<time> <`.
#[derive(Clone, Copy)]
struct Bounds {
    has_from: bool,
    has_to: bool,
}

/// What may come next in `WHERE`: a whole condition with the next free
/// `$n`, or what joins and follows conditions.
fn condition_candidates(
    view: Option<&View>,
    all: &[String],
    conditions: &[String],
    selected: &[String],
) -> Vec<String> {
    match conditions.last() {
        Some(last) if last.starts_with('$') => after_a_condition(selected),
        Some(last) if !is(last, "AND") => Vec::new(),
        _ => match view {
            // Without the view, the time's name is unknown.
            None => Vec::new(),
            Some(view) => {
                let bounds = Bounds {
                    has_from: has_bound(&view.time, all, ">="),
                    has_to: has_bound(&view.time, all, "<"),
                };
                next_conditions(view, all, conditions, bounds)
            }
        },
    }
}

/// What may follow a whole condition: `AND`, or the end of the query.
fn after_a_condition(selected: &[String]) -> Vec<String> {
    let mut next = vec!["AND ".to_owned()];
    next.extend(query_ends(selected));
    next
}

/// What ends a query once its conditions are written, if it has any:
/// `GROUP BY` the selected keys, then `;` or `HAVING`.
fn query_ends(selected: &[String]) -> Vec<String> {
    if selected.is_empty() {
        return vec!["; ".to_owned(), "HAVING ".to_owned()];
    }
    let keys = selected.join(", ");
    vec![
        format!("GROUP BY {keys}; "),
        format!("GROUP BY {keys} HAVING "),
    ]
}

/// What may come next in `HAVING`: a selected measure, then a comparison
/// with the next free `$n`, then what joins and ends conditions.
fn having_candidates(view: Option<&View>, all: &[String], conditions: &[String]) -> Vec<String> {
    let Some(view) = view else {
        return Vec::new();
    };
    let number = next_parameter_number(all);
    match conditions.last() {
        Some(last) if last.starts_with('$') => vec!["AND ".to_owned(), "; ".to_owned()],
        Some(last) if view.has_measure(last) => {
            let mut next = Vec::with_capacity(COMPARISONS.len());
            for comparison in COMPARISONS {
                next.push(format!("{comparison} {number}"));
            }
            next
        }
        Some(last) if !is(last, "AND") => Vec::new(),
        _ => selected_measures(view, all),
    }
}

/// The measures between `SELECT` and `FROM`, the ones HAVING may compare,
/// each with the space after it.
fn selected_measures(view: &View, all: &[String]) -> Vec<String> {
    let mut selected = Vec::new();
    for word in selected_words(all) {
        if view.has_measure(&word) {
            selected.push(format!("{word} "));
        }
    }
    selected
}

/// The conditions that may start here: the bounds not yet written, then
/// each key not yet conditioned.
fn next_conditions(
    view: &View,
    all: &[String],
    conditions: &[String],
    bounds: Bounds,
) -> Vec<String> {
    let number = next_parameter_number(all);
    let time = &view.time;
    let mut next = match (bounds.has_from, bounds.has_to) {
        (false, false) => {
            let after = number.next();
            vec![format!("{time} >= {number} AND {time} < {after}")]
        }
        (false, true) => vec![format!("{time} >= {number}")],
        (true, false) => vec![format!("{time} < {number}")],
        (true, true) => Vec::new(),
    };
    for key in &view.keys {
        if key.name != *time && !conditions.contains(&key.name) {
            next.push(format!("{} = {number}", key.name));
        }
    }
    next
}

fn parameter_number(word: &str) -> Option<ParameterNumber> {
    let digits = word.strip_prefix('$')?;
    let Ok(number) = digits.parse() else {
        return None;
    };
    Some(ParameterNumber(number))
}

fn next_parameter_number(words: &[String]) -> ParameterNumber {
    let mut highest = ParameterNumber(0);
    for word in words {
        if let Some(number) = parameter_number(word) {
            highest = highest.max(number);
        }
    }
    highest.next()
}

/// What a parameter is the value of: the key a query compares `$n` with.
struct Slot {
    number: ParameterNumber,
    key: Key,
    operator: String,
    view: View,
}

impl Slot {
    /// The values Tab can give: the buckets held for a bound, both
    /// booleans for a boolean.
    fn values(&self) -> Vec<String> {
        let is_time = self.key.name == self.view.time;
        let bound = match (is_time, self.operator.as_str()) {
            (true, ">=") => self.view.from.clone(),
            (true, "<") => self.view.to.clone(),
            _ => None,
        };
        let values = match (bound, self.key.column_type.as_str()) {
            (Some(bound), _) => vec![bound],
            (None, "boolean") => vec!["true".to_owned(), "false".to_owned()],
            (None, _) => Vec::new(),
        };
        let mut candidates = Vec::with_capacity(values.len());
        for value in values {
            candidates.push(format!("{value} "));
        }
        candidates
    }
}

/// The slot of the parameter after those `given`, by the `$n` it is.
fn slot(views: &[View], query: &str, given: &str) -> Option<Slot> {
    let number = ParameterNumber(parameter_words(given).len() + 1);
    let query = words(query);
    let view = view_after_from(views, &query)?;
    let mut key: Option<Key> = None;
    let mut operator = String::new();
    for word in &query {
        if let Some(named) = view.key(word) {
            key = Some(named.clone());
        } else if view.has_measure(word) {
            // `/views` gives no measure's type.
            key = Some(Key {
                name: word.clone(),
                column_type: String::new(),
            });
        } else if COMPARISONS.contains(&word.as_str()) || is(word, "IN") {
            operator.clone_from(word);
        } else if parameter_number(word) == Some(number) {
            return Some(Slot {
                number,
                key: key?,
                operator,
                view,
            });
        }
    }
    None
}

/// What the next parameter is, shown past the cursor while none of it is
/// typed.
pub(crate) fn hint(views: &[View], line: &str, cursor: usize) -> Option<String> {
    let (query, given) = line.split_once(';')?;
    if cursor != line.len() {
        return None;
    }
    let typing = match given.chars().next_back() {
        Some(last) => !last.is_whitespace(),
        None => false,
    };
    if typing || has_open_quote(given) {
        return None;
    }
    let slot = slot(views, query, given)?;
    let Key { name, column_type } = &slot.key;
    let gap = if given.is_empty() { " " } else { "" };
    let tab = match slot.values().first() {
        Some(value) if slot.key.name == slot.view.time => {
            format!(", Tab gives {}", value.trim_end())
        }
        _ => String::new(),
    };
    // In parentheses and worded as a sentence, so it can't be taken for
    // text to type: `$1: status = text` was typed as `$1 = ...`.
    let value = if column_type.is_empty() {
        "value"
    } else {
        column_type.as_str()
    };
    Some(format!(
        "{gap}(the {value} for {name} {} {}{tab})",
        slot.operator, slot.number
    ))
}

#[cfg(test)]
mod tests;
