//! What Tab offers in `straum shell`, by where in the line the cursor is: a
//! view after `FROM`, its keys and measures after `SELECT`, a whole
//! condition after `WHERE`, the selected keys after `GROUP BY`, the bounds
//! of the buckets held where a parameter is a bound. Plain functions over
//! the line and the views a Straum serves as JSON; no terminal and no
//! network. A line is split into words, never parsed: Straum checks the
//! ask.

use serde_json::Value;

const COMMANDS: [&str; 4] = ["\\d", "\\rows ", "\\?", "\\q"];
const BUCKET: &str = "bucket";

/// A view as `GET /views` describes it in JSON: what a line may name.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct View {
    name: String,
    /// `bucket` first.
    keys: Vec<Key>,
    measures: Vec<String>,
    /// The start of the first bucket held and the end of the last.
    from: Option<String>,
    to: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Key {
    name: String,
    column_type: String,
}

/// `None` for JSON that isn't the views.
pub(crate) fn read_views(json: &str) -> Option<Vec<View>> {
    let json: Value = serde_json::from_str(json).ok()?;
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let views = json.get("views")?.as_array()?.iter().map(|view| {
        let keys = view.get("keys")?.as_array()?.iter().map(|key| {
            let column_type = text(key.get("type")?)?;
            Some(Key {
                name: text(key.get("name")?)?,
                column_type: match key.get("scale").and_then(Value::as_u64) {
                    Some(scale) => format!("{column_type} scale {scale}"),
                    None => column_type,
                },
            })
        });
        Some(View {
            name: text(view.get("name")?)?,
            keys: keys.collect::<Option<_>>()?,
            measures: (view.get("measures")?.as_array()?.iter())
                .map(text)
                .collect::<Option<_>>()?,
            from: view.get("from").and_then(text),
            to: view.get("to").and_then(text),
        })
    });
    views.collect()
}

/// What Tab offers: each candidate replaces the line from `start` to the
/// cursor.
pub(crate) struct Completion {
    pub(crate) start: usize,
    pub(crate) candidates: Vec<String>,
}

fn parts(c: char) -> bool {
    c.is_whitespace() || ",()".contains(c)
}

/// The words of a query: names, keywords, `$n` and operators. Commas and
/// parentheses only part them.
fn words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    // Where the word being read starts, and whether it is an operator.
    let mut word: Option<(usize, bool)> = None;
    for (at, c) in text.char_indices() {
        let kind = (!parts(c)).then(|| "<>=".contains(c));
        if let Some((start, operator)) = word
            && kind != Some(operator)
        {
            words.push(&text[start..at]);
            word = None;
        }
        if word.is_none() {
            word = kind.map(|operator| (at, operator));
        }
    }
    words.extend(word.map(|(start, _)| &text[start..]));
    words
}

fn is(word: &str, keyword: &str) -> bool {
    word.eq_ignore_ascii_case(keyword)
}

fn after<'a, 'w>(words: &'a [&'w str], keyword: &str) -> Option<&'a [&'w str]> {
    let at = words.iter().rposition(|word| is(word, keyword))?;
    Some(&words[at + 1..])
}

/// Where the word the cursor is in starts.
fn word_start(before: &str, parts: fn(char) -> bool) -> usize {
    let word: usize = (before.chars().rev())
        .take_while(|&c| !parts(c))
        .map(char::len_utf8)
        .sum();
    before.len() - word
}

pub(crate) fn complete(views: &[View], line: &str, pos: usize) -> Completion {
    let before = &line[..pos];
    let (start, candidates) = match before.split_once(';') {
        Some((query, _)) => {
            // A parameter starts no earlier than the semicolon ends.
            let start = word_start(before, char::is_whitespace).max(query.len() + 1);
            let slot = slot(views, query, &before[query.len() + 1..start]);
            (start, slot.map_or_else(Vec::new, |slot| slot.values()))
        }
        None => {
            let start = word_start(before, parts);
            (start, query_candidates(views, line, &before[..start]))
        }
    };
    let typed = &before[start..];
    let fits = |candidate: &String| {
        (candidate.get(..typed.len())).is_some_and(|head| head.eq_ignore_ascii_case(typed))
    };
    Completion {
        start,
        candidates: candidates.into_iter().filter(fits).collect(),
    }
}

/// What may come next in a query, after the words `before` the cursor's.
fn query_candidates(views: &[View], line: &str, before: &str) -> Vec<String> {
    let before = words(before);
    let all = words(line);
    let Some(first) = before.first() else {
        return std::iter::once("SELECT ")
            .chain(COMMANDS)
            .map(str::to_owned)
            .collect();
    };
    if is(first, "\\rows") && before.len() == 1 {
        return views.iter().map(|view| view.name.clone()).collect();
    }
    let view = (after(&all, "FROM").and_then(|rest| rest.first()))
        .and_then(|name| views.iter().find(|view| view.name == *name));
    let selected = selected_keys(view, &all);

    if let Some(listed) = after(&before, "GROUP") {
        return match listed {
            [] => vec!["BY ".to_owned()],
            [_by] => vec![selected.join(", ")],
            [_by, listed @ ..] => {
                let left = selected.iter().filter(|key| !listed.contains(key));
                let left: Vec<String> = left.map(|key| (*key).to_owned()).collect();
                if left.is_empty() {
                    vec!["; ".to_owned()]
                } else {
                    left
                }
            }
        };
    }
    if let Some(conditions) = after(&before, "WHERE") {
        return condition_candidates(view, &all, conditions, &selected);
    }
    if let Some(named) = after(&before, "FROM") {
        return match named {
            [] => views.iter().map(|view| format!("{} ", view.name)).collect(),
            _ => vec!["WHERE ".to_owned()],
        };
    }
    if let Some(listed) = after(&before, "SELECT") {
        // Before FROM names the view, any view's names may be meant.
        let of = |view: &View| {
            let keys = view.keys.iter().map(|key| key.name.clone());
            keys.chain(view.measures.iter().cloned())
                .collect::<Vec<_>>()
        };
        let mut names: Vec<String> = Vec::new();
        for name in view.map_or_else(|| views.iter().flat_map(of).collect(), of) {
            if !names.contains(&name) && !listed.contains(&name.as_str()) {
                names.push(name);
            }
        }
        if !listed.is_empty() {
            names.push("FROM ".to_owned());
        }
        return names;
    }
    Vec::new()
}

/// The keys between `SELECT` and `FROM`, which `GROUP BY` names again.
fn selected_keys<'w>(view: Option<&View>, all: &[&'w str]) -> Vec<&'w str> {
    let Some(view) = view else {
        return Vec::new();
    };
    let listed = after(all, "SELECT").unwrap_or_default();
    (listed.iter().take_while(|word| !is(word, "FROM")))
        .filter(|word| view.keys.iter().any(|key| key.name == **word))
        .copied()
        .collect()
}

fn has_bound(all: &[&str], operator: &str) -> bool {
    all.windows(2)
        .any(|pair| pair[0] == BUCKET && pair[1] == operator)
}

/// What may come next in `WHERE`: a whole condition with the next free
/// `$n`, or what joins and follows conditions.
fn condition_candidates(
    view: Option<&View>,
    all: &[&str],
    conditions: &[&str],
    selected: &[&str],
) -> Vec<String> {
    let (from, to) = (has_bound(all, ">="), has_bound(all, "<"));
    match conditions.last() {
        Some(last) if last.starts_with('$') => {
            let mut next = vec!["AND ".to_owned()];
            if from && to {
                next.push(if selected.is_empty() {
                    "; ".to_owned()
                } else {
                    format!("GROUP BY {}; ", selected.join(", "))
                });
            }
            next
        }
        Some(last) if !is(last, "AND") => Vec::new(),
        _ => {
            let numbered = all.iter().filter_map(|word| word.strip_prefix('$'));
            let n = 1
                + (numbered.filter_map(|n| n.parse::<u32>().ok()))
                    .max()
                    .unwrap_or(0);
            let mut next = match (from, to) {
                (false, false) => {
                    vec![format!("{BUCKET} >= ${n} AND {BUCKET} < ${}", n + 1)]
                }
                (false, true) => vec![format!("{BUCKET} >= ${n}")],
                (true, false) => vec![format!("{BUCKET} < ${n}")],
                (true, true) => Vec::new(),
            };
            let keys = view.map_or(&[][..], |view| &view.keys[..]);
            let free = (keys.iter().map(|key| key.name.as_str()))
                .filter(|name| *name != BUCKET && !conditions.contains(name));
            next.extend(free.map(|name| format!("{name} = ${n}")));
            next
        }
    }
}

/// What a parameter is the value of: the key a query compares `$n` with.
struct Slot<'v> {
    number: usize,
    key: &'v Key,
    operator: &'v str,
    view: &'v View,
}

impl Slot<'_> {
    /// The values Tab can give: the buckets held for a bound, both
    /// booleans for a boolean.
    fn values(&self) -> Vec<String> {
        let bound = match (self.key.name.as_str(), self.operator) {
            (BUCKET, ">=") => self.view.from.as_ref(),
            (BUCKET, "<") => self.view.to.as_ref(),
            _ => None,
        };
        let values: Vec<&str> = match (bound, self.key.column_type.as_str()) {
            (Some(bound), _) => vec![bound],
            (None, "boolean") => vec!["true", "false"],
            (None, _) => Vec::new(),
        };
        values.iter().map(|value| format!("{value} ")).collect()
    }
}

/// The slot of the parameter after those `given`, by the `$n` it is.
fn slot<'v>(views: &'v [View], query: &'v str, given: &str) -> Option<Slot<'v>> {
    let number = given.split_whitespace().count() + 1;
    let query = words(query);
    let name = after(&query, "FROM")?.first()?;
    let view = views.iter().find(|view| view.name == *name)?;
    let (mut key, mut operator) = (None, "");
    for word in &query {
        if let Some(named) = view.keys.iter().find(|key| key.name == *word) {
            key = Some(named);
        } else if ["=", ">=", "<"].contains(word) || is(word, "IN") {
            operator = word;
        } else if word.strip_prefix('$').and_then(|n| n.parse().ok()) == Some(number) {
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
pub(crate) fn hint(views: &[View], line: &str, pos: usize) -> Option<String> {
    let (query, given) = line.split_once(';')?;
    if pos != line.len() || !given.chars().next_back().is_none_or(char::is_whitespace) {
        return None;
    }
    let slot = slot(views, query, given)?;
    let Key { name, column_type } = slot.key;
    let gap = if given.is_empty() { " " } else { "" };
    let tab = match slot.values().first() {
        Some(value) if slot.key.name == BUCKET => format!(", Tab gives {}", value.trim_end()),
        _ => String::new(),
    };
    Some(format!(
        "{gap}${}: {name} {} {column_type}{tab}",
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

    #[test]
    fn reads_the_views_a_straum_serves() {
        let views = held();
        assert_eq!(views.len(), 2);
        let key = |name: &str, column_type: &str| Key {
            name: name.to_owned(),
            column_type: column_type.to_owned(),
        };
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
            ["AND ", "; "]
        );
        assert_eq!(
            offers(&format!(
                "SELECT status, country, orders FROM orders {bounds}"
            )),
            ["AND ", "GROUP BY status, country; "]
        );
        assert_eq!(
            offers(&format!("SELECT bucket, orders FROM orders {bounds}g")),
            ["GROUP BY bucket; "]
        );
    }

    #[test]
    fn group_by_offers_the_selected_keys() {
        let line = "SELECT status, country, orders FROM orders \
                    WHERE bucket >= $1 AND bucket < $2 GROUP ";
        assert_eq!(offers(line), ["BY "]);
        assert_eq!(offers(&format!("{line}BY ")), ["status, country"]);
        assert_eq!(offers(&format!("{line}BY country, ")), ["status"]);
        assert_eq!(offers(&format!("{line}BY country, status ")), ["; "]);
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
        let hinted = |line: &str| hint(&held(), line, line.len());
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
        assert_eq!(words(" ,( "), Vec::<&str>::new());
        // The cursor's word starts after the last part, whatever its width.
        assert_eq!(tab("SELECT blå"), ("blå".to_owned(), Vec::new()));
    }
}
