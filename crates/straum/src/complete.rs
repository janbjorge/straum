//! What Tab offers in `straum shell`, and the hint shown past the cursor.
//! `read_views` reads the views a Straum serves as JSON; `complete` and
//! `hint` take those views and the line being edited and look at the words
//! before the cursor: a view after `FROM`, its keys and measures after
//! `SELECT`, a whole condition after `WHERE`, the selected keys after
//! `GROUP BY`, a selected measure and a comparison after `HAVING`, the
//! bounds of the buckets held where a parameter is a bound. Plain functions, with no terminal and no network: `shell.rs`
//! calls them. A line is split into words, never parsed: Straum checks the
//! ask.

use std::fmt;

use serde_json::Value;

use crate::client::{has_open_quote, parameter_words};

const COMMANDS: [&str; 4] = ["\\d", "\\rows ", "\\?", "\\q"];
const BUCKET: &str = "bucket";
/// What HAVING compares a measure with, as the ask writes it.
const COMPARISONS: [&str; 6] = ["=", "<>", "<", "<=", ">", ">="];

/// A view as `GET /views` describes it in JSON: what a line may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct View {
    name: String,
    /// `bucket` first.
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
    fn next(self) -> ParameterNumber {
        ParameterNumber(self.0 + 1)
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
    for candidate in offered.candidates {
        if starts_with_ignoring_case(&candidate, typed) {
            candidates.push(candidate);
        }
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
    if is(first, "\\rows") && before.len() == 1 {
        return view_names(views, "");
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
            return view_names(views, " ");
        }
        return vec!["WHERE ".to_owned()];
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

/// Each view's name, followed by `ending`.
fn view_names(views: &[View], ending: &str) -> Vec<String> {
    let mut names = Vec::with_capacity(views.len());
    for view in views {
        names.push(format!("{}{ending}", view.name));
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
    let listed = after(all, "SELECT").unwrap_or_default();
    let mut selected = Vec::new();
    for word in listed {
        if is(&word, "FROM") {
            break;
        }
        if view.key(&word).is_some() {
            selected.push(word);
        }
    }
    selected
}

fn has_bound(all: &[String], operator: &str) -> bool {
    for pair in all.windows(2) {
        if pair[0] == BUCKET && pair[1] == operator {
            return true;
        }
    }
    false
}

/// Which bounds of the range the query has written: `bucket >=` and
/// `bucket <`.
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
    let bounds = Bounds {
        has_from: has_bound(all, ">="),
        has_to: has_bound(all, "<"),
    };
    match conditions.last() {
        Some(last) if last.starts_with('$') => after_a_condition(bounds, selected),
        Some(last) if !is(last, "AND") => Vec::new(),
        _ => next_conditions(view, all, conditions, bounds),
    }
}

/// What may follow a whole condition: `AND`, and once both bounds are
/// written, the end of the query.
fn after_a_condition(bounds: Bounds, selected: &[String]) -> Vec<String> {
    let mut next = vec!["AND ".to_owned()];
    if bounds.has_from && bounds.has_to {
        if selected.is_empty() {
            next.push("; ".to_owned());
            next.push("HAVING ".to_owned());
        } else {
            let keys = selected.join(", ");
            next.push(format!("GROUP BY {keys}; "));
            next.push(format!("GROUP BY {keys} HAVING "));
        }
    }
    next
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
    let listed = after(all, "SELECT").unwrap_or_default();
    let mut selected = Vec::new();
    for word in listed {
        if is(&word, "FROM") {
            break;
        }
        if view.has_measure(&word) {
            selected.push(format!("{word} "));
        }
    }
    selected
}

/// The conditions that may start here: the bounds not yet written, then
/// each key not yet conditioned.
fn next_conditions(
    view: Option<&View>,
    all: &[String],
    conditions: &[String],
    bounds: Bounds,
) -> Vec<String> {
    let number = next_parameter_number(all);
    let mut next = match (bounds.has_from, bounds.has_to) {
        (false, false) => {
            let after = number.next();
            vec![format!("{BUCKET} >= {number} AND {BUCKET} < {after}")]
        }
        (false, true) => vec![format!("{BUCKET} >= {number}")],
        (true, false) => vec![format!("{BUCKET} < {number}")],
        (true, true) => Vec::new(),
    };
    let Some(view) = view else {
        return next;
    };
    for key in &view.keys {
        if key.name != BUCKET && !conditions.contains(&key.name) {
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
        let bound = match (self.key.name.as_str(), self.operator.as_str()) {
            (BUCKET, ">=") => self.view.from.clone(),
            (BUCKET, "<") => self.view.to.clone(),
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
        Some(value) if slot.key.name == BUCKET => format!(", Tab gives {}", value.trim_end()),
        _ => String::new(),
    };
    let column_type = if column_type.is_empty() {
        String::new()
    } else {
        format!(" {column_type}")
    };
    Some(format!(
        "{gap}{}: {name} {}{column_type}{tab}",
        slot.number, slot.operator
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWS: &str = r#"{"views":[
        {"name":"orders",
         "from":"2026-01-01T09:00:00Z","to":"2026-01-01T11:00:00Z",
         "keys":[{"name":"bucket","type":"timestamptz"},{"name":"status","type":"text"},
                 {"name":"shipped","type":"boolean"},{"name":"country","type":"text"}],
         "measures":["orders","revenue"]},
        {"name":"logins",
         "from":null,"to":null,
         "keys":[{"name":"bucket","type":"timestamp"},{"name":"country","type":"text"}],
         "measures":["logins"]}]}"#;

    fn held() -> Vec<View> {
        read_views(VIEWS).unwrap()
    }

    /// The candidates at the end of `line`, and the text they replace.
    fn tab(line: &str) -> (String, Vec<String>) {
        let Completion { start, candidates } = complete(&held(), line, line.len());
        (line[start..].to_owned(), candidates)
    }

    fn offers(line: &str) -> Vec<String> {
        tab(line).1
    }

    /// The hint with the cursor at the end of `line`.
    fn hinted(line: &str) -> Option<String> {
        hint(&held(), line, line.len())
    }

    fn key(name: &str, column_type: &str) -> Key {
        Key {
            name: name.to_owned(),
            column_type: column_type.to_owned(),
        }
    }

    #[test]
    fn reads_the_views_a_straum_serves() {
        let views = held();
        assert_eq!(views.len(), 2);
        assert_eq!(
            views[1],
            View {
                name: "logins".to_owned(),
                keys: vec![key("bucket", "timestamp"), key("country", "text")],
                measures: vec!["logins".to_owned()],
                from: None,
                to: None,
            }
        );
        assert_eq!(views[0].from.as_deref(), Some("2026-01-01T09:00:00Z"));
        // A numeric says its scale beside its type.
        let scaled = r#"{"views":[{"name":"v","measures":[],
            "keys":[{"name":"fare","type":"numeric","scale":2}]}]}"#;
        assert_eq!(
            read_views(scaled).unwrap()[0].keys,
            [key("fare", "numeric scale 2")]
        );
        for not_views in [
            "",
            "[]",
            "{}",
            r#"{"views":{}}"#,
            r#"{"views":[{}]}"#,
            r#"{"views":[{"name":1}]}"#,
            r#"{"views":[{"name":"v"}]}"#,
            r#"{"views":[{"name":"v","keys":[{}]}]}"#,
            r#"{"views":[{"name":"v","keys":[{"name":"k"}]}]}"#,
            r#"{"views":[{"name":"v","keys":[]}]}"#,
            r#"{"views":[{"name":"v","keys":[],"measures":[1]}]}"#,
        ] {
            assert_eq!(read_views(not_views), None, "{not_views}");
        }
    }

    #[test]
    fn a_scale_that_is_not_a_number_is_left_out() {
        let scaled = r#"{"views":[{"name":"v","measures":[],
            "keys":[{"name":"fare","type":"numeric","scale":"two"}]}]}"#;
        assert_eq!(
            read_views(scaled).unwrap()[0].keys,
            [key("fare", "numeric")]
        );
    }

    #[test]
    fn a_line_starts_with_select_or_a_command() {
        assert_eq!(offers(""), ["SELECT ", "\\d", "\\rows ", "\\?", "\\q"]);
        assert_eq!(tab("se"), ("se".to_owned(), vec!["SELECT ".to_owned()]));
        assert_eq!(offers("\\"), ["\\d", "\\rows ", "\\?", "\\q"]);
        assert_eq!(offers("\\r"), ["\\rows "]);
        assert_eq!(offers("\\rows "), ["orders", "logins"]);
        assert_eq!(offers("\\rows lo"), ["logins"]);
        assert_eq!(offers("\\rows logins "), Vec::<String>::new());
        assert_eq!(offers("\\d "), Vec::<String>::new());
    }

    #[test]
    fn select_offers_the_names_not_yet_listed() {
        // Any view's names, each once, until FROM says which view.
        assert_eq!(
            offers("SELECT "),
            [
                "bucket", "status", "shipped", "country", "orders", "revenue", "logins"
            ]
        );
        assert_eq!(offers("select s"), ["status", "shipped"]);
        assert_eq!(
            offers("SELECT status, "),
            [
                "bucket", "shipped", "country", "orders", "revenue", "logins", "FROM "
            ]
        );
        assert_eq!(offers("SELECT status,o"), ["orders"]);
        assert_eq!(offers("SELECT status, orders f"), ["FROM "]);
        // With the view named further on, only its names.
        let (typed, candidates) = tab("SELECT ");
        assert_eq!(typed, "");
        assert!(candidates.contains(&"logins".to_owned()));
        let line = "SELECT  FROM logins";
        let Completion { candidates, .. } = complete(&held(), line, 7);
        assert_eq!(candidates, ["bucket", "country", "logins"]);
    }

    #[test]
    fn from_offers_the_views_then_where() {
        assert_eq!(offers("SELECT orders FROM "), ["orders ", "logins "]);
        assert_eq!(offers("SELECT orders FROM lo"), ["logins "]);
        assert_eq!(offers("SELECT orders FROM orders "), ["WHERE "]);
        assert_eq!(offers("SELECT orders FROM orders w"), ["WHERE "]);
        assert_eq!(
            offers("SELECT orders FROM orders WHERE x"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn where_offers_whole_conditions_with_the_next_parameter() {
        let line = "SELECT status, orders FROM orders WHERE ";
        assert_eq!(
            offers(line),
            [
                "bucket >= $1 AND bucket < $2",
                "status = $1",
                "shipped = $1",
                "country = $1"
            ]
        );
        assert_eq!(
            tab(&format!("{line}bu")),
            (
                "bu".to_owned(),
                vec!["bucket >= $1 AND bucket < $2".to_owned()]
            )
        );
        // A key is conditioned once, and a bound written is not offered.
        let line = format!("{line}status = $1 AND ");
        assert_eq!(
            offers(&line),
            [
                "bucket >= $2 AND bucket < $3",
                "shipped = $2",
                "country = $2"
            ]
        );
        let line = format!("{line}bucket >= $2 AND ");
        assert_eq!(
            offers(&line),
            ["bucket < $3", "shipped = $3", "country = $3"]
        );
        assert_eq!(
            offers("SELECT orders FROM orders WHERE bucket<$1 AND "),
            [
                "bucket >= $2",
                "status = $2",
                "shipped = $2",
                "country = $2"
            ]
        );
    }

    #[test]
    fn where_offers_nothing_mid_condition_or_for_an_unknown_view() {
        // In the middle of a condition there is nothing to offer.
        assert_eq!(
            offers("SELECT orders FROM orders WHERE status "),
            Vec::<String>::new()
        );
        // A view Straum doesn't serve has only its bounds.
        assert_eq!(
            offers("SELECT x FROM nope WHERE "),
            ["bucket >= $1 AND bucket < $2"]
        );
        // With both bounds written and no view, nothing is left.
        assert_eq!(
            offers("SELECT x FROM nope WHERE bucket >= $1 AND bucket < $2 AND "),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_finished_condition_offers_what_follows() {
        let bounds = "WHERE bucket >= $1 AND bucket < $2 ";
        assert_eq!(
            offers("SELECT orders FROM orders WHERE status = $1 "),
            ["AND "]
        );
        assert_eq!(
            offers(&format!("SELECT orders FROM orders {bounds}")),
            ["AND ", "; ", "HAVING "]
        );
        assert_eq!(
            offers(&format!(
                "SELECT status, country, orders FROM orders {bounds}"
            )),
            [
                "AND ",
                "GROUP BY status, country; ",
                "GROUP BY status, country HAVING "
            ]
        );
        assert_eq!(
            offers(&format!("SELECT bucket, orders FROM orders {bounds}g")),
            ["GROUP BY bucket; ", "GROUP BY bucket HAVING "]
        );
    }

    #[test]
    fn group_by_offers_the_selected_keys() {
        let line = "SELECT status, country, orders FROM orders \
                    WHERE bucket >= $1 AND bucket < $2 GROUP ";
        assert_eq!(offers(line), ["BY "]);
        assert_eq!(offers(&format!("{line}BY ")), ["status, country"]);
        assert_eq!(offers(&format!("{line}BY country, ")), ["status"]);
        assert_eq!(
            offers(&format!("{line}BY country, status ")),
            ["; ", "HAVING "]
        );
    }

    #[test]
    fn having_offers_selected_measures_then_comparisons() {
        let grouped = "SELECT status, orders, revenue FROM orders \
                       WHERE bucket >= $1 AND bucket < $2 GROUP BY status ";
        assert_eq!(offers(grouped), ["; ", "HAVING "]);
        let having = format!("{grouped}HAVING ");
        assert_eq!(offers(&having), ["orders ", "revenue "]);
        assert_eq!(
            offers(&format!("{having}orders ")),
            ["= $3", "<> $3", "< $3", "<= $3", "> $3", ">= $3"]
        );
        assert_eq!(offers(&format!("{having}orders > $3 ")), ["AND ", "; "]);
        assert_eq!(
            offers(&format!("{having}orders > $3 AND ")),
            ["orders ", "revenue "]
        );
        // Nothing mid-condition, for a key, or for an unknown view.
        assert_eq!(offers(&format!("{having}orders > ")), Vec::<String>::new());
        assert_eq!(offers(&format!("{having}status ")), Vec::<String>::new());
        assert_eq!(
            offers("SELECT x FROM nope WHERE bucket >= $1 AND bucket < $2 HAVING "),
            Vec::<String>::new()
        );
        assert_eq!(
            hinted(&format!("{having}revenue >= $3; a b ")).as_deref(),
            Some("$3: revenue >=")
        );
    }

    #[test]
    fn a_parameter_that_is_a_bound_offers_the_buckets_held() {
        let query = "SELECT orders FROM orders WHERE shipped = $3 \
                     AND bucket >= $1 AND bucket < $2";
        assert_eq!(offers(&format!("{query}; ")), ["2026-01-01T09:00:00Z "]);
        assert_eq!(offers(&format!("{query};")), ["2026-01-01T09:00:00Z "]);
        assert_eq!(
            tab(&format!("{query}; 2026-01-01T09:00:00Z 20")),
            ("20".to_owned(), vec!["2026-01-01T11:00:00Z ".to_owned()])
        );
        assert_eq!(offers(&format!("{query}; a b ")), ["true ", "false "]);
        assert_eq!(offers(&format!("{query}; a b f")), ["false "]);
    }

    #[test]
    fn a_quoted_value_is_one_parameter() {
        let query = "SELECT orders FROM orders WHERE shipped = $3 \
                     AND bucket >= $1 AND bucket < $2";
        assert_eq!(
            offers(&format!("{query}; 'a b' 'c d' ")),
            ["true ", "false "]
        );
        assert_eq!(
            hinted(&format!("{query}; 'a b' 'c d' ")).as_deref(),
            Some("$3: shipped = boolean")
        );
    }

    #[test]
    fn inside_an_open_quote_nothing_is_offered_or_hinted() {
        let query = "SELECT orders FROM orders WHERE shipped = $3 \
                     AND bucket >= $1 AND bucket < $2";
        for open in ["; 'a b ", "; a 'b c ", "; a 'b c f"] {
            let line = format!("{query}{open}");
            assert_eq!(offers(&line), Vec::<String>::new(), "{open}");
            assert_eq!(hinted(&line), None, "{open}");
        }
    }

    #[test]
    fn a_parameter_offers_nothing_where_nothing_is_known() {
        let query = "SELECT orders FROM orders WHERE shipped = $3 \
                     AND bucket >= $1 AND bucket < $2";
        // Past the last parameter, and where nothing is known.
        assert_eq!(offers(&format!("{query}; a b c ")), Vec::<String>::new());
        assert_eq!(
            offers("SELECT orders FROM orders WHERE status = $1; "),
            Vec::<String>::new()
        );
        let logins = "SELECT logins FROM logins WHERE bucket >= $1 AND bucket < $2; ";
        assert_eq!(offers(logins), Vec::<String>::new());
        assert_eq!(
            offers("SELECT x FROM nope WHERE bucket >= $1; "),
            Vec::<String>::new()
        );
        assert_eq!(
            offers("SELECT x WHERE bucket >= $1; "),
            Vec::<String>::new()
        );
        assert_eq!(
            offers("SELECT orders FROM orders WHERE $1; "),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_hint_says_what_the_next_parameter_is() {
        let query = "SELECT orders FROM orders WHERE status IN ($3, $4) \
                     AND bucket >= $1 AND bucket < $2";
        assert_eq!(
            hinted(&format!("{query};")).as_deref(),
            Some(" $1: bucket >= timestamptz, Tab gives 2026-01-01T09:00:00Z")
        );
        assert_eq!(
            hinted(&format!("{query}; x ")).as_deref(),
            Some("$2: bucket < timestamptz, Tab gives 2026-01-01T11:00:00Z")
        );
        assert_eq!(
            hinted(&format!("{query}; x y ")).as_deref(),
            Some("$3: status IN text")
        );
        assert_eq!(
            hinted(&format!("{query}; x y z ")).as_deref(),
            Some("$4: status IN text")
        );
        let logins = "SELECT logins FROM logins WHERE bucket >= $1 AND bucket < $2; ";
        assert_eq!(hinted(logins).as_deref(), Some("$1: bucket >= timestamp"));
        // None while a parameter is being typed, past the last one, in
        // the query, or with the cursor back in the line.
        assert_eq!(hinted(&format!("{query}; x")), None);
        assert_eq!(hinted(&format!("{query}; a b c d ")), None);
        assert_eq!(hinted(query), None);
        let line = format!("{query}; ");
        assert_eq!(hint(&held(), &line, 3), None);
    }

    #[test]
    fn words_part_at_commas_parentheses_and_operators() {
        assert_eq!(
            words("a,b (c)  d>=$1 AND e<$2 x = y"),
            [
                "a", "b", "c", "d", ">=", "$1", "AND", "e", "<", "$2", "x", "=", "y"
            ]
        );
        assert_eq!(words(" ,( "), Vec::<String>::new());
        // The cursor's word starts after the last part, whatever its width.
        assert_eq!(tab("SELECT blå"), ("blå".to_owned(), Vec::new()));
    }

    #[test]
    fn a_candidate_fits_what_is_typed_in_any_case() {
        assert!(starts_with_ignoring_case("SELECT ", "sel"));
        assert!(starts_with_ignoring_case("SELECT ", ""));
        // Typed past the candidate's end, or into the middle of a letter.
        assert!(!starts_with_ignoring_case("SEL", "SELECT"));
        assert!(!starts_with_ignoring_case("blåbær", "bla"));
        assert!(!starts_with_ignoring_case("FROM ", "SEL"));
    }
}
