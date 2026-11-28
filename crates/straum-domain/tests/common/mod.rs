//! Helpers the test binaries share: builders for spec pieces, the SQL
//! rules a reference needs (`=`, each aggregate over a group's values, a
//! bucket's start and an ask's range), and a seeded random number source.
//! Each reference reads its own columns into a [`GroupInputs`], and
//! [`aggregate_value`] answers a measure from it.

// Every test binary compiles this module and uses only some of it.
// `#[expect(dead_code)]` would fail in a binary that uses all of it,
// so this is the one `allow` AGENTS.md permits.
#![allow(dead_code)]

use straum_domain::{
    Aggregate, Answer, Ask, AskKey, Attribute, ByKey, Condition, Datum, Dim, Fraction, Identifier,
    Literal, Measure, Start, Store, Table, Value, Width, Window,
};

/// Asserts that a window of half the rows of `whole`, from row 1 and from
/// the end, builds the rows that `whole` has there. Such a window shows
/// fewer rows than the groups, so the store reads the cells again for
/// its rows. An ask with HAVING does not, so this checks none.
pub fn windows_match(store: &Store, ask: &Ask, whole: &Answer) {
    let total = whole.rows.len();
    if !ask.having.is_empty() || total < 2 {
        return;
    }
    let count = total / 2;
    let starts = [
        (Start::FromFirst(1), 1),
        (Start::FromLast(count + 1), total - count - 1),
    ];
    for (start, first) in starts {
        let window = Window { start, count };
        let cut = store.ask_window(ask, Some(window)).unwrap();
        assert_eq!(cut.cut.total, total, "{window:?}, {ask:?}");
        let shown = &whole.rows[first..first + count];
        assert_eq!(cut.answer.rows, shown, "{window:?}, {ask:?}");
    }
}

/// Whether a row at `at` is in the ask's range, as PostgreSQL reads
/// `bucket >= from AND bucket < to`; an open end leaves its side unbounded.
pub fn in_range(ask: &Ask, at: i64) -> bool {
    let after_start = match ask.from {
        Some(from) => at >= from,
        None => true,
    };
    let before_end = match ask.to {
        Some(to) => at < to,
        None => true,
    };
    after_start && before_end
}

/// The start of the bucket a bucket key names for a row at `at`: the
/// view's own `width` for its bucket, the width asked for a wider one.
pub fn bucket_key(key: &AskKey, width: Width, at: i64) -> Datum {
    let width = match key {
        AskKey::WiderBucket { width, .. } => *width,
        AskKey::Key(_) => width,
    };
    Datum::Int(width.bucket_start(at).unwrap())
}

/// What a reference reads of one group for one measure: the rows that
/// pass the measure's filter, and the values of its input among them.
pub struct GroupInputs {
    /// Rows in the group.
    pub rows: usize,
    /// The input's integers, one per row that has one.
    pub ints: Vec<i128>,
    /// The input's booleans, one per row that has one.
    pub bools: Vec<bool>,
    /// Rows whose input is not null, of any type.
    pub not_null: usize,
    /// Distinct values of the input that are not null, of any type.
    pub distinct: usize,
}

/// A measure over one group, as SQL works it out from the group's inputs.
pub fn aggregate_value(aggregate: &Aggregate, group: GroupInputs) -> Value {
    match aggregate {
        Aggregate::Count => Value::Int(group.rows as i128),
        Aggregate::CountNotNull(_) => Value::Int(group.not_null as i128),
        Aggregate::CountDistinct(_) => Value::Int(group.distinct as i128),
        Aggregate::Sum(_) => sum(&group.ints),
        Aggregate::Avg(_) => average(&group.ints),
        Aggregate::Min(_) => int_or_null(group.ints.into_iter().min()),
        Aggregate::Max(_) => int_or_null(group.ints.into_iter().max()),
        Aggregate::BoolAnd(_) => bool_or_null(group.bools.into_iter().min()),
        Aggregate::BoolOr(_) => bool_or_null(group.bools.into_iter().max()),
        Aggregate::PercentileDisc { fraction, .. } => percentile_disc(group.ints, *fraction),
        Aggregate::Mode(_) => mode(group.ints),
        Aggregate::SumDistinct(_) => distinct(&group.ints, Distinct::Sum),
        Aggregate::AvgDistinct(_) => distinct(&group.ints, Distinct::Avg),
        Aggregate::VarSamp(_)
        | Aggregate::VarPop(_)
        | Aggregate::StddevSamp(_)
        | Aggregate::StddevPop(_) => spread(aggregate, &group.ints),
    }
}

/// `sum(c)`: null for no value.
fn sum(values: &[i128]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut total = 0;
    for value in values {
        total += value;
    }
    Value::Int(total)
}

/// `avg(c)`, as its sum and count: null for no value.
fn average(values: &[i128]) -> Value {
    let Value::Int(total) = sum(values) else {
        return Value::Null;
    };
    Value::Avg {
        sum: total,
        count: values.len() as u64,
    }
}

/// `percentile_disc(units / 10^places)` over `column`.
pub fn percentile(units: u64, places: u32, column: &str) -> Aggregate {
    Aggregate::PercentileDisc {
        fraction: Fraction::new(units, places).unwrap(),
        column: id(column),
    }
}

/// PostgreSQL's `percentile_disc`: of the values sorted, the one at
/// `ceil(fraction * n)` counting from 1, the product in float8.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "PostgreSQL takes the product in float8 too, and the rank is from 0 to the count"
)]
pub fn percentile_disc(mut values: Vec<i128>, fraction: Fraction) -> Value {
    values.sort_unstable();
    let fraction = fraction.units as f64 / 10f64.powi(i32::try_from(fraction.places).unwrap());
    let rank = (fraction * values.len() as f64).ceil() as usize;
    match values.get(rank.max(1) - 1) {
        Some(&value) => Value::Int(value),
        None => Value::Null,
    }
}

/// PostgreSQL's `mode()`: the value most rows hold, the least of those
/// tied, as it walks the values sorted; null for none.
pub fn mode(mut values: Vec<i128>) -> Value {
    values.sort_unstable();
    let mut best: Option<(i128, usize)> = None;
    let mut start = 0;
    while start < values.len() {
        let mut end = start;
        while end < values.len() && values[end] == values[start] {
            end += 1;
        }
        let count = end - start;
        if best.is_none_or(|(_, most)| count > most) {
            best = Some((values[start], count));
        }
        start = end;
    }
    match best {
        Some((value, _)) => Value::Int(value),
        None => Value::Null,
    }
}

/// `sum(DISTINCT c)` or, with [`Distinct::Avg`], `avg(DISTINCT c)` of
/// `values`: each value once; null for none.
pub fn distinct(values: &[i128], of: Distinct) -> Value {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        seen.insert(*value);
    }
    if seen.is_empty() {
        return Value::Null;
    }
    let mut sum = 0;
    for value in &seen {
        sum += value;
    }
    match of {
        Distinct::Sum => Value::Int(sum),
        Distinct::Avg => Value::Avg {
            sum,
            count: seen.len() as u64,
        },
    }
}

/// Which aggregate of distinct values [`distinct`] gives.
#[derive(Debug, Clone, Copy)]
pub enum Distinct {
    Sum,
    Avg,
}

/// Places past its scale a variance or deviation answers to.
pub const SPREAD_PLACES: u32 = 4;

/// A variance or deviation by its definition, in a pass over the values
/// apart from the one that sums them: with `n` values and sum `s1`, the
/// squared deviations from the mean are `sum((n * x - s1)^2) / n^2`.
/// Never through `n * s2 - s1^2`, the form Straum keeps, so a wrong
/// merge or formula can't agree with it by sharing it. Rounded half to
/// even at [`SPREAD_PLACES`]; null for no value, or a sample of one.
pub fn spread(aggregate: &Aggregate, values: &[i128]) -> Value {
    let Some(parts) = spread_parts(aggregate, values) else {
        return Value::Null;
    };
    let factor = 10_i128.pow(SPREAD_PLACES);
    if parts.root {
        Value::Int(nearest_root(
            parts.squared * factor * factor,
            parts.denominator,
        ))
    } else {
        Value::Int(nearest(parts.squared * factor, parts.denominator))
    }
}

/// A variance by its definition, exact: `squared / denominator` in the
/// column's units, squared. A deviation is the root of that.
pub struct SpreadParts {
    pub squared: i128,
    pub denominator: i128,
    pub root: bool,
}

/// The exact [`SpreadParts`] that [`spread`] rounds; `None` for no
/// value, or a sample of one.
pub fn spread_parts(aggregate: &Aggregate, values: &[i128]) -> Option<SpreadParts> {
    let count = values.len() as i128;
    let mut sum = 0;
    for value in values {
        sum += value;
    }
    let mut squared = 0;
    for value in values {
        let apart = count * value - sum;
        squared += apart * apart;
    }
    let (denominator, root) = match aggregate {
        Aggregate::VarSamp(_) => (count * count * (count - 1), false),
        Aggregate::VarPop(_) => (count * count * count, false),
        Aggregate::StddevSamp(_) => (count * count * (count - 1), true),
        Aggregate::StddevPop(_) => (count * count * count, true),
        other => panic!("{other:?} is no variance or deviation"),
    };
    if denominator <= 0 {
        return None;
    }
    Some(SpreadParts {
        squared,
        denominator,
        root,
    })
}

/// `numerator / denominator`, both at least zero, rounded half to even.
fn nearest(numerator: i128, denominator: i128) -> i128 {
    let whole = numerator / denominator;
    let twice_rest = 2 * (numerator % denominator);
    if twice_rest > denominator || (twice_rest == denominator && whole % 2 == 1) {
        whole + 1
    } else {
        whole
    }
}

/// The whole `u` nearest `sqrt(numerator / denominator)`, ties to even:
/// the least `u` whose upper halfway square, `(u + 1/2)^2`, is not below
/// the quotient, found by halving.
fn nearest_root(numerator: i128, denominator: i128) -> i128 {
    let (mut low, mut high) = (0, 1);
    while !halfway_reaches(high, numerator, denominator) {
        high *= 2;
    }
    while low < high {
        let middle = low.midpoint(high);
        if halfway_reaches(middle, numerator, denominator) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    let tie = (2 * low + 1) * (2 * low + 1) * denominator == 4 * numerator;
    if tie && low % 2 == 1 { low + 1 } else { low }
}

/// Whether `(root + 1/2)^2` reaches `numerator / denominator`.
fn halfway_reaches(root: i128, numerator: i128, denominator: i128) -> bool {
    (2 * root + 1) * (2 * root + 1) * denominator >= 4 * numerator
}

/// A measure named `name` with no `FILTER`.
pub fn measure(name: &str, aggregate: Aggregate) -> Measure {
    Measure {
        name: id(name),
        aggregate,
        filter: vec![],
    }
}

/// `value` as an integer cell, `NULL` when there is none.
pub fn int_or_null(value: Option<i128>) -> Value {
    match value {
        Some(value) => Value::Int(value),
        None => Value::Null,
    }
}

/// `value` as a boolean cell, `NULL` when there is none.
pub fn bool_or_null(value: Option<bool>) -> Value {
    match value {
        Some(value) => Value::Bool(value),
        None => Value::Null,
    }
}

/// An identifier known to be valid.
pub fn id(name: &str) -> Identifier {
    Identifier::new(name).unwrap()
}

/// Identifiers known to be valid, one per name.
pub fn names(names: &[&str]) -> Vec<Identifier> {
    let mut identifiers = Vec::with_capacity(names.len());
    for name in names {
        identifiers.push(id(name));
    }
    identifiers
}

/// A dim over `public.name`, keyed by the `key` columns, with the
/// `attributes` a join may read.
pub fn dim(name: &str, key: &[&str], attributes: &[&str]) -> Dim {
    Dim {
        as_text: Vec::new(),
        name: id(name),
        table: table(name),
        key: names(key),
        attributes: names(attributes),
    }
}

/// A by key that reads the column of the same name.
pub fn by_key(name: &str) -> ByKey {
    ByKey::Column {
        column: id(name),
        name: id(name),
    }
}

/// A dim column a join reads, named `name` in the view.
pub fn attribute(name: &str, column: &str) -> Attribute {
    Attribute {
        name: id(name),
        column: id(column),
    }
}

/// A text value.
pub fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}

/// A text literal.
pub fn lit(value: &str) -> Literal {
    Literal::Text(value.into())
}

/// `key IN (values)`.
pub fn condition(key: &str, values: Vec<Literal>) -> Condition {
    Condition {
        key: id(key),
        values,
    }
}

/// `public.name`.
pub fn table(name: &str) -> Table {
    Table {
        schema: id("public"),
        name: id(name),
    }
}

/// A dim row as a load reads it: the key, then the attributes.
pub fn dim_row(key: &[Datum], attributes: &[Datum]) -> Vec<Datum> {
    [key, attributes].concat()
}

/// SQL `=`: null matches nothing.
pub fn holds(value: &Datum, values: &[Literal]) -> bool {
    for literal in values {
        if equals(value, literal) {
            return true;
        }
    }
    false
}

fn equals(value: &Datum, literal: &Literal) -> bool {
    match (value, literal) {
        (Datum::Int(found), Literal::Int(wanted)) => found == wanted,
        (Datum::Text(found), Literal::Text(wanted)) => **found == **wanted,
        (Datum::Bool(found), Literal::Bool(wanted)) => found == wanted,
        _ => false,
    }
}

/// A splitmix64 sequence from its seed, the one field; the tests need no
/// crate for randomness. The unit tests in `src/` have their own copy,
/// which these tests can't reach.
pub struct Rng(pub u64);

impl Rng {
    /// The next number of the sequence.
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }

    /// A number from 0 up to, not including, `bound`.
    pub fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound as u64).unwrap()
    }

    /// True `percent` percent of the time.
    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// An integer in `low..high`.
    pub fn int(&mut self, low: i64, high: i64) -> i64 {
        let offset = self.next() % high.abs_diff(low);
        low.checked_add_unsigned(offset).unwrap()
    }

    /// An integer in `low..high`, or null `percent` percent of the time.
    pub fn maybe(&mut self, low: i64, high: i64, percent: usize) -> Datum {
        if self.chance(percent) {
            Datum::Null
        } else {
            Datum::Int(self.int(low, high))
        }
    }

    /// One item of `items`.
    // Generic, and borrowing the item: the tests pick from lists of
    // names, rows and kinds alike, and use the pick in place.
    pub fn pick<'a, Item>(&mut self, items: &'a [Item]) -> &'a Item {
        &items[self.below(items.len())]
    }
}
