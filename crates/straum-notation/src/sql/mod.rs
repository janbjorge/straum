//! The walker: it reads the tree `sqlparser` builds from Straum SQL and
//! turns it into Straum's own types. `view` walks a spec's `CREATE VIEW`
//! statements into dims and facts, `ask` walks an ask's `SELECT` into an
//! `Ask`; `definition` cuts a spec's text into its statements as written.
//! This file holds the `Walker`, which collects every error it
//! meets, and the parts both walks share: names, tables, the one `SELECT`
//! and its clauses, and filter conditions. `calls` holds the function
//! calls both walks read, `buckets` among them.
//!
//! Deny by default: every `match` lists what Straum SQL accepts and rejects the
//! rest by name, and accepted structs are destructured without `..`, so a
//! field added by a `sqlparser` upgrade fails to compile until it is handled
//! here.

use std::collections::HashSet;
use std::fmt;

use sqlparser::ast as sql;
use sqlparser::ast::Spanned;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};
use straum_domain::{Condition, Identifier, Literal};

mod ask;
mod calls;
mod definition;
mod view;

use ask::Parameters;
pub use ask::parse_ask;
pub(crate) use definition::statement_texts;
use view::Scope;
pub(crate) use view::{Views, parse};

/// How much of a rejected construct an error quotes before it cuts.
const QUOTE_CHARACTERS: usize = 60;

/// How a refusal that quotes or names a construct ends, so no one takes
/// it for PostgreSQL's error. An error that says what to write instead,
/// such as `write <> for not equal, not !=`, goes without it.
const NOT_IN_STRAUM_SQL: &str = "is not in Straum SQL, the PostgreSQL subset made only for Straum";

/// One thing wrong with a spec or an ask, and the line it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    /// 1-based; none when `sqlparser` puts the position in `message`.
    pub line: Option<u64>,
    pub message: String,
}

/// An error the walker has already put in its list. A walk that returns it
/// stops there, and its caller adds no error of its own for the same node.
#[derive(Debug)]
struct Reported;

/// The most operators one statement may hold, counted before it is
/// parsed. `sqlparser` builds a chain of them, `a AND b AND ...`,
/// `1 + 1 + ...` or `SELECT ... UNION SELECT ...`, as a tree as deep as
/// the chain, and the walk, a node's span and the tree's own drop recurse
/// down it: 2,500 overflowed the 2 MiB stack an ask is read on, and
/// aborted Straum. No view or ask needs near this many.
const MAX_OPERATORS: usize = 512;

fn statements(text: &str) -> Result<Vec<sql::Statement>, Vec<SqlError>> {
    let dialect = PostgreSqlDialect {};
    let tokenized = Tokenizer::new(&dialect, text)
        .with_unescape(true)
        .tokenize_with_location();
    let tokens = match tokenized {
        Ok(tokens) => tokens,
        Err(error) => return Err(parser_error(&ParserError::from(error))),
    };
    if let Some(error) = too_many_operators(&tokens) {
        return Err(vec![error]);
    }
    if let Some(error) = other_operator_spelling(&tokens) {
        return Err(vec![error]);
    }
    let mut parser = Parser::new(&dialect).with_tokens_with_locations(tokens);
    let statements = parser
        .parse_statements()
        .map_err(|error| parser_error(&error))?;
    // `sqlparser` stops at an `END` after a statement and reads no further.
    let left = parser.peek_token();
    if left.token != Token::EOF {
        return Err(vec![SqlError {
            line: Some(left.span.start.line),
            message: format!("`{}` after a statement {NOT_IN_STRAUM_SQL}", left.token),
        }]);
    }
    Ok(statements)
}

/// `sqlparser`'s error, which knows no line of its own. It quotes the
/// token that it found whole, so the token is cut as a quote is.
fn parser_error(error: &ParserError) -> Vec<SqlError> {
    let message = error.to_string();
    let message = match message.split_once(", found: ") {
        Some((expected, found)) => {
            let (token, at) = found.split_at(found.rfind(" at Line: ").unwrap_or(found.len()));
            format!("{expected}, found: {}{at}", cut(token))
        }
        None => message,
    };
    vec![SqlError {
        line: None,
        message,
    }]
}

/// `text` cut after [`QUOTE_CHARACTERS`] characters: a name or a value
/// of an ask may be a mebibyte long, and an error quotes only its start.
fn cut(text: &str) -> String {
    match text.char_indices().nth(QUOTE_CHARACTERS) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_owned(),
    }
}

/// The error for the first statement of `tokens` with more than
/// [`MAX_OPERATORS`] operators, at the line of the one past it.
fn too_many_operators(tokens: &[TokenWithSpan]) -> Option<SqlError> {
    let mut operators = 0;
    for token in tokens {
        if token.token == Token::SemiColon {
            operators = 0;
        } else if is_operator(&token.token) {
            operators += 1;
        }
        if operators > MAX_OPERATORS {
            return Some(SqlError {
                line: Some(token.span.start.line),
                message: format!(
                    "more than {MAX_OPERATORS} operators in one statement; \
                     Straum SQL takes fewer"
                ),
            });
        }
    }
    None
}

/// The error for the first `==` or `NOTNULL` of `tokens`, naming the one
/// spelling. `sqlparser` reads `==` as `=`, but PostgreSQL has no `==`.
/// `NOTNULL` is PostgreSQL's second spelling of `IS NOT NULL`.
fn other_operator_spelling(tokens: &[TokenWithSpan]) -> Option<SqlError> {
    // After a `.` or `AS`, PostgreSQL reads `notnull` as a name.
    let mut names_next = false;
    for token in tokens {
        let message = match &token.token {
            Token::Whitespace(_) => continue,
            Token::DoubleEq => "write = for equal, not ==",
            Token::Word(word) if word.keyword == Keyword::NOTNULL && !names_next => {
                "write IS NOT NULL, not NOTNULL"
            }
            Token::Word(word) => {
                names_next = word.keyword == Keyword::AS;
                continue;
            }
            other => {
                names_next = *other == Token::Period;
                continue;
            }
        };
        return Some(SqlError {
            line: Some(token.span.start.line),
            message: message.into(),
        });
    }
    None
}

/// A token that can join or follow an expression or a query and so deepen
/// its tree: a symbol other than a comma, a parenthesis or a period, a
/// keyword such as `AND`, `IS`, `IN` or `OPERATOR`, or a set operation
/// such as `UNION`. Literals, names and parameters are not.
fn is_operator(token: &Token) -> bool {
    match token {
        Token::Word(word) => matches!(
            word.keyword,
            Keyword::AND
                | Keyword::OR
                | Keyword::NOT
                | Keyword::IS
                | Keyword::IN
                | Keyword::LIKE
                | Keyword::ILIKE
                | Keyword::SIMILAR
                | Keyword::BETWEEN
                | Keyword::COLLATE
                | Keyword::AT
                | Keyword::OVERLAPS
                | Keyword::MEMBER
                | Keyword::NOTNULL
                | Keyword::OPERATOR
                | Keyword::REGEXP
                | Keyword::RLIKE
                | Keyword::XOR
                | Keyword::UNION
                | Keyword::EXCEPT
                | Keyword::INTERSECT
                | Keyword::MINUS
        ),
        Token::EOF
        | Token::Number(..)
        | Token::SingleQuotedString(_)
        | Token::DoubleQuotedString(_)
        | Token::DollarQuotedString(_)
        | Token::NationalStringLiteral(_)
        | Token::EscapedStringLiteral(_)
        | Token::UnicodeStringLiteral(_)
        | Token::HexStringLiteral(_)
        | Token::Placeholder(_)
        | Token::Whitespace(_)
        | Token::Comma
        | Token::LParen
        | Token::RParen
        | Token::Period
        | Token::SemiColon => false,
        _ => true,
    }
}

// Any node of the tree: each kind knows where it was written.
fn line(node: &impl Spanned) -> u64 {
    node.span().start.line
}

const CONDITION: &str = "filter condition, write = or IN joined by AND,";
const ASK_COMPARISON: &str = "comparison, WHERE takes = or IN on a key, >= and < on the \
                              time; compare a measure in HAVING,";
const PARENTHESES: &str = "parentheses, write the condition without them,";

/// The most errors a walk keeps. A 1 MiB ask can hold half a million
/// mistakes, and keeping and printing each made a 74 MB reply; the rest
/// are counted.
const MAX_ERRORS: usize = 50;

/// Walks one spec or one ask, keeping its first [`MAX_ERRORS`] errors in
/// the order met, so a reader sees them at once, and counting the rest.
#[derive(Default)]
struct Walker {
    errors: Vec<SqlError>,
    /// Errors met past the ones kept.
    more: usize,
}

/// What a condition's keys and values are read against: a view's scope,
/// whose columns reach the keys that read them, or an ask's parameters,
/// the keys named outright.
// Borrows the scope or the parameters for one condition, so it needs a
// lifetime; they belong to the walk that holds them.
enum Names<'walk> {
    View(&'walk Scope, Clause),
    Ask(&'walk mut Parameters),
}

/// Which clause of a view a condition is in, so an error names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Clause {
    /// The view's `WHERE`.
    Where,
    /// A measure's `FILTER (WHERE ..)`.
    Filter,
}

/// Which null test a condition or a null-ness key writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NullTest {
    IsNull,
    IsNotNull,
}

impl Walker {
    fn error(&mut self, line: u64, message: String) {
        if self.errors.len() == MAX_ERRORS {
            self.more += 1;
            return;
        }
        // `sqlparser` gives an empty span line 0.
        let line = if line == 0 { None } else { Some(line) };
        self.errors.push(SqlError { line, message });
    }

    /// How many errors the walk has met, kept or counted.
    fn met(&self) -> usize {
        self.errors.len() + self.more
    }

    /// The walk's value, or the errors it kept and how many more it met.
    // Generic over what was walked: a spec's views or an ask.
    fn finish<T>(mut self, value: Result<T, Reported>) -> Result<T, Vec<SqlError>> {
        match value {
            Ok(value) if self.errors.is_empty() => Ok(value),
            _ => {
                if self.more > 0 {
                    self.errors.push(SqlError {
                        line: None,
                        message: format!("and {} more errors", self.more),
                    });
                }
                Err(self.errors)
            }
        }
    }

    // Any node of the tree, printed as the error's quote.
    fn refuse(&mut self, line: u64, what: &str, node: &impl fmt::Display) {
        let text = node.to_string();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        self.not_in_subset(line, &format!("{what} `{}`", cut(&text)));
    }

    /// The plain GROUP BY list, the only form a view or an ask uses.
    // Borrows the list from the tree, which outlives the walk.
    fn group_by_expressions<'tree>(
        &mut self,
        at: u64,
        group_by: &'tree sql::GroupByExpr,
    ) -> Result<&'tree [sql::Expr], Reported> {
        match group_by {
            sql::GroupByExpr::Expressions(expressions, modifiers) => {
                self.reject_if_present(at, !modifiers.is_empty(), "ROLLUP, CUBE or GROUPING SETS");
                Ok(expressions)
            }
            sql::GroupByExpr::All(_) => {
                self.not_in_subset(at, "GROUP BY ALL");
                Err(Reported)
            }
        }
    }

    /// Rejects `clause` when the statement writes it.
    // One call per clause keeps the long lists of rejected clauses one
    // line each; many are clauses only other dialects parse, so an `if`
    // per clause leaves lines no test can reach (measured with
    // `cargo llvm-cov`).
    fn reject_if_present(&mut self, line: u64, present: bool, clause: &str) {
        if present {
            self.not_in_subset(line, clause);
        }
    }

    /// Every rejection says Straum SQL is Straum's own subset, so no one
    /// takes it for PostgreSQL's error.
    fn not_in_subset(&mut self, line: u64, what: &str) {
        self.error(line, format!("{what} {NOT_IN_STRAUM_SQL}"));
    }

    /// An unquoted name, folded to lower case as PostgreSQL does: ASCII
    /// letters only, so the Kelvin sign is no `k`.
    fn read_identifier(&mut self, identifier: &sql::Ident) -> Result<Identifier, Reported> {
        let sql::Ident {
            value,
            quote_style,
            span,
        } = identifier;
        if quote_style.is_some() {
            self.refuse(span.start.line, "quoted identifier", identifier);
            return Err(Reported);
        }
        let lower = value.to_ascii_lowercase();
        if *value != lower {
            self.error(
                span.start.line,
                format!("{}: write names in lowercase, {}", cut(value), cut(&lower)),
            );
            return Err(Reported);
        }
        let Some(read) = Identifier::new(value) else {
            self.error(span.start.line, format!("bad identifier {}", cut(value)));
            return Err(Reported);
        };
        Ok(read)
    }

    /// A key's or measure's name, `AS name`, which asks must be able to
    /// say.
    fn read_output_name(&mut self, alias: &sql::Ident) -> Result<Identifier, Reported> {
        let name = self.read_identifier(alias)?;
        if !ask::askable(&name) {
            self.error(
                alias.span.start.line,
                format!(
                    "{name} is a word SQL reserves, so no ask could name it; name it otherwise"
                ),
            );
            return Err(Reported);
        }
        Ok(name)
    }

    fn bare_name(&mut self, name: &sql::ObjectName, what: &str) -> Result<Identifier, Reported> {
        let sql::ObjectName(parts) = name;
        let [part] = parts.as_slice() else {
            let quoted = cut(&name.to_string());
            self.error(line(name), format!("{what} {quoted} must be a bare name"));
            return Err(Reported);
        };
        self.name_part(name, part)
    }

    fn name_part(
        &mut self,
        name: &sql::ObjectName,
        part: &sql::ObjectNamePart,
    ) -> Result<Identifier, Reported> {
        match part {
            sql::ObjectNamePart::Identifier(identifier) => self.read_identifier(identifier),
            other @ sql::ObjectNamePart::Function(_) => {
                self.refuse(line(name), "name part", other);
                Err(Reported)
            }
        }
    }

    /// A table by name, after rejecting every other part of a table
    /// reference by name; what the alias may be is the caller's.
    // Borrows the name and alias from the tree, which outlives the walk.
    fn table_factor<'tree>(
        &mut self,
        factor: &'tree sql::TableFactor,
    ) -> Result<NamedTable<'tree>, Reported> {
        let sql::TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_directives,
            inheritance,
        } = factor
        else {
            self.refuse(line(factor), "table", factor);
            return Err(Reported);
        };
        let at = line(factor);
        self.reject_if_present(at, args.is_some(), "a table function");
        self.reject_if_present(at, !with_hints.is_empty(), "a table hint");
        self.reject_if_present(at, version.is_some(), "a table version");
        self.reject_if_present(at, *with_ordinality, "WITH ORDINALITY");
        self.reject_if_present(at, !partitions.is_empty(), "PARTITION");
        self.reject_if_present(at, json_path.is_some(), "a JSON path");
        self.reject_if_present(at, sample.is_some(), "TABLESAMPLE");
        self.reject_if_present(at, !index_directives.is_empty(), "an index directive");
        self.reject_if_present(at, inheritance.is_some(), "ONLY or *");
        Ok(NamedTable {
            name,
            alias: alias.as_ref(),
        })
    }

    /// The one `SELECT` of a query, after rejecting every clause around it.
    // Borrows the `SELECT` from the tree, which outlives the walk.
    fn one_select<'tree>(
        &mut self,
        query: &'tree sql::Query,
        what: &str,
    ) -> Result<&'tree sql::Select, Reported> {
        let sql::Query {
            with,
            body,
            order_by,
            limit_clause,
            fetch,
            locks,
            for_clause,
            settings,
            format_clause,
            pipe_operators,
        } = query;
        let at = line(query);
        self.reject_if_present(at, with.is_some(), "WITH (a CTE)");
        self.reject_if_present(at, order_by.is_some(), "ORDER BY");
        self.reject_if_present(at, limit_clause.is_some(), "LIMIT or OFFSET");
        self.reject_if_present(at, fetch.is_some(), "FETCH");
        self.reject_if_present(at, !locks.is_empty(), "FOR UPDATE or FOR SHARE");
        self.reject_if_present(at, for_clause.is_some(), "FOR");
        self.reject_if_present(at, settings.is_some(), "SETTINGS");
        self.reject_if_present(at, format_clause.is_some(), "FORMAT");
        self.reject_if_present(at, !pipe_operators.is_empty(), "a pipe operator");
        match body.as_ref() {
            sql::SetExpr::Select(select) => Ok(select),
            other => {
                self.refuse(at, &format!("{what}, use one SELECT,"), other);
                Err(Reported)
            }
        }
    }

    /// Rejects every `SELECT` clause but the list, FROM, WHERE, GROUP BY
    /// and HAVING, which each caller reads its own way. Returns its line.
    fn select_clauses(&mut self, select: &sql::Select) -> u64 {
        let sql::Select {
            select_token,
            optimizer_hints,
            distinct,
            select_modifiers,
            top,
            // Only says where TOP was written, which is rejected.
            top_before_distinct: _,
            projection: _,
            exclude,
            into,
            from: _,
            lateral_views,
            prewhere,
            selection: _,
            connect_by,
            group_by: _,
            cluster_by,
            distribute_by,
            sort_by,
            having: _,
            named_window,
            qualify,
            // Only says where QUALIFY was written, which is rejected.
            window_before_qualify: _,
            value_table_mode,
            flavor,
        } = select;
        let at = select_token.0.span.start.line;
        self.reject_if_present(at, !optimizer_hints.is_empty(), "an optimizer hint");
        match distinct {
            None => {}
            Some(sql::Distinct::All) => self.not_in_subset(at, "SELECT ALL"),
            Some(sql::Distinct::Distinct | sql::Distinct::On(_)) => {
                self.not_in_subset(at, "SELECT DISTINCT");
            }
        }
        self.reject_if_present(at, select_modifiers.is_some(), "a SELECT modifier");
        self.reject_if_present(at, top.is_some(), "TOP");
        self.reject_if_present(at, exclude.is_some(), "EXCLUDE");
        self.reject_if_present(at, into.is_some(), "SELECT INTO");
        self.reject_if_present(at, !lateral_views.is_empty(), "LATERAL VIEW");
        self.reject_if_present(at, prewhere.is_some(), "PREWHERE");
        self.reject_if_present(at, !connect_by.is_empty(), "CONNECT BY");
        self.reject_if_present(at, !cluster_by.is_empty(), "CLUSTER BY");
        self.reject_if_present(at, !distribute_by.is_empty(), "DISTRIBUTE BY");
        self.reject_if_present(at, !sort_by.is_empty(), "SORT BY");
        self.reject_if_present(at, !named_window.is_empty(), "WINDOW");
        self.reject_if_present(at, qualify.is_some(), "QUALIFY");
        self.reject_if_present(at, value_table_mode.is_some(), "SELECT AS VALUE or STRUCT");
        match flavor {
            sql::SelectFlavor::Standard => {}
            sql::SelectFlavor::FromFirst | sql::SelectFlavor::FromFirstNoSelect => {
                self.not_in_subset(at, "FROM before SELECT");
            }
        }
        at
    }

    /// `key = value` and `key IN (values)` joined by `AND`, and in a view
    /// `column IS NULL` and `IS NOT NULL`, which count through a null-ness
    /// key; an ask names that key outright, `key = $n`.
    fn read_conditions(
        &mut self,
        expression: &sql::Expr,
        conditions: &mut Vec<Condition>,
        names: &mut Names,
    ) {
        let mut parts = Vec::new();
        and_parts(expression, &mut parts);
        for part in parts {
            match part {
                sql::Expr::IsNull(column) | sql::Expr::IsNotNull(column) => {
                    let Names::View(scope, clause) = names else {
                        self.refuse(line(part), CONDITION, part);
                        continue;
                    };
                    let test = match part {
                        sql::Expr::IsNull(_) => NullTest::IsNull,
                        _ => NullTest::IsNotNull,
                    };
                    if let Ok(condition) = self.null_condition(scope, *clause, part, column, test) {
                        conditions.push(condition);
                    }
                }
                sql::Expr::BinaryOp {
                    left,
                    op: sql::BinaryOperator::Eq,
                    right,
                } => {
                    // Both sides are walked before either is judged, so each
                    // reports its own errors.
                    let key = self.condition_key(names, left);
                    let value = self.condition_value(names, right);
                    if let Ok(key) = key
                        && let Ok(value) = value
                    {
                        conditions.push(Condition {
                            key,
                            values: vec![value],
                        });
                    }
                }
                sql::Expr::In {
                    expr: inner,
                    target: sql::InTarget::List(list),
                    negated: false,
                    global: false,
                } => {
                    if list.len() < 2 {
                        self.refuse(line(part), "IN with one value, write =,", part);
                        continue;
                    }
                    if let Ok(condition) = self.in_list(names, inner, list) {
                        conditions.push(condition);
                    }
                }
                sql::Expr::Nested(_) => self.refuse(line(part), PARENTHESES, part),
                other => {
                    let what = match names {
                        Names::Ask(_) if compares_a_name(other) => ASK_COMPARISON,
                        _ => CONDITION,
                    };
                    self.refuse(line(other), what, other);
                }
            }
        }
    }

    /// The parts, each written again after its first refused by name: the
    /// same key, operator and value twice is one condition spelled twice.
    // Borrows the parts from the tree, which outlives the walk.
    fn without_repeats<'tree>(&mut self, parts: Vec<&'tree sql::Expr>) -> Vec<&'tree sql::Expr> {
        let mut seen: HashSet<String> = HashSet::with_capacity(parts.len());
        let mut kept = Vec::with_capacity(parts.len());
        for part in parts {
            let written = part.to_string();
            if seen.contains(&written) {
                self.error(line(part), format!("{} given twice", cut(&written)));
                continue;
            }
            seen.insert(written);
            kept.push(part);
        }
        kept
    }

    /// `key IN (values)`, every value walked even after one fails; a value
    /// written again is refused by name, as `without_repeats` refuses a
    /// condition. `IN (1, 1)` says only that 1 is given twice: once
    /// dropped, the one-value IN names its own spelling.
    fn in_list(
        &mut self,
        names: &mut Names,
        key: &sql::Expr,
        list: &[sql::Expr],
    ) -> Result<Condition, Reported> {
        let key = self.condition_key(names, key);
        let mut values: Vec<Literal> = Vec::new();
        let mut seen: HashSet<String> = HashSet::with_capacity(list.len());
        let mut failed = false;
        for item in list {
            let written = item.to_string();
            if seen.contains(&written) {
                self.error(line(item), format!("{} given twice", cut(&written)));
                failed = true;
                continue;
            }
            seen.insert(written);
            match self.condition_value(names, item) {
                Ok(value) => values.push(value),
                Err(Reported) => failed = true,
            }
        }
        let key = key?;
        if failed {
            return Err(Reported);
        }
        Ok(Condition { key, values })
    }

    fn condition_key(
        &mut self,
        names: &mut Names,
        expression: &sql::Expr,
    ) -> Result<Identifier, Reported> {
        match names {
            Names::View(scope, clause) => self.key_of(scope, expression, *clause),
            Names::Ask(_) => self.ask_name(expression),
        }
    }

    fn condition_value(
        &mut self,
        names: &mut Names,
        expression: &sql::Expr,
    ) -> Result<Literal, Reported> {
        match names {
            Names::View(..) => self.literal(expression),
            Names::Ask(parameters) => self.parameter(expression, parameters),
        }
    }
}

/// `name <> value`, `<`, `<=`, `>` or `>=`: what HAVING writes, so
/// likely meant for a measure.
fn compares_a_name(expression: &sql::Expr) -> bool {
    let sql::Expr::BinaryOp { left, op, .. } = expression else {
        return false;
    };
    if !matches!(left.as_ref(), sql::Expr::Identifier(_)) {
        return false;
    }
    let compares = matches!(
        op,
        sql::BinaryOperator::NotEq
            | sql::BinaryOperator::Lt
            | sql::BinaryOperator::LtEq
            | sql::BinaryOperator::Gt
            | sql::BinaryOperator::GtEq
    );
    compares
}

/// `digits` as its one spelling, `7` for `007`, when written with leading
/// zeros; `None` when it has none, or is zeros only and so no number
/// Straum SQL counts from 1.
fn without_leading_zeros(digits: &str) -> Option<&str> {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.len() == digits.len() || trimmed.is_empty() {
        return None;
    }
    Some(trimmed)
}

/// A number as written, its digits and any point, with no sign.
fn written_digits(expression: &sql::Expr) -> Option<&str> {
    let sql::Expr::Value(sql::ValueWithSpan {
        value: sql::Value::Number(digits, false),
        span: _,
    }) = expression
    else {
        return None;
    };
    Some(digits)
}

/// Whether `text` is ASCII digits only, with no sign; an empty text is.
fn all_digits(text: &str) -> bool {
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return false;
        }
    }
    true
}

/// The byte offset of each `sqlparser` location in a text. Lines and
/// columns count from 1, as `sqlparser` counts them, and columns in
/// characters, not bytes.
// One pass over the text, then a binary search for each location. A seek
// from the start of the text for each location made a long ask quadratic.
pub(super) struct Offsets {
    /// The character index at which each line starts.
    line_starts: Vec<usize>,
    /// A step at the start of the text and after each character of more
    /// than one byte. Only one-byte characters come between two steps.
    steps: Vec<Step>,
    /// The byte length of the text.
    end: usize,
}

/// A character index and its byte offset.
#[derive(Clone, Copy)]
struct Step {
    characters: usize,
    offset: usize,
}

impl Offsets {
    pub(super) fn of(text: &str) -> Offsets {
        let mut line_starts = vec![0];
        let mut steps = vec![Step {
            characters: 0,
            offset: 0,
        }];
        for (characters, (offset, character)) in text.char_indices().enumerate() {
            if character == '\n' {
                line_starts.push(characters + 1);
            } else if !character.is_ascii() {
                steps.push(Step {
                    characters: characters + 1,
                    offset: offset + character.len_utf8(),
                });
            }
        }
        Offsets {
            line_starts,
            steps,
            end: text.len(),
        }
    }

    /// Where `location` is, as a byte offset; the end of the text for a
    /// location past it.
    pub(super) fn offset(&self, location: Location) -> usize {
        let line = usize::try_from(location.line.saturating_sub(1)).unwrap_or(usize::MAX);
        let Some(&line_start) = self.line_starts.get(line) else {
            return self.end;
        };
        let column = usize::try_from(location.column.saturating_sub(1)).unwrap_or(usize::MAX);
        let characters = line_start.saturating_add(column);
        // The first step is at character 0, so one step is at or before.
        let after = self
            .steps
            .partition_point(|step| step.characters <= characters);
        let step = self.steps[after - 1];
        let ascii = characters - step.characters;
        step.offset.saturating_add(ascii).min(self.end)
    }
}

/// The text from `start` to `end`, located as sqlparser does.
// The lifetime says that the text comes from `text`, not from `offsets`.
pub(super) fn written_between<'text>(
    text: &'text str,
    offsets: &Offsets,
    start: Location,
    end: Location,
) -> &'text str {
    let start = offsets.offset(start);
    // An end before the start is no text, never a panic.
    let end = offsets.offset(end).max(start);
    &text[start..end]
}

/// The items printed and joined by `, `.
// Any printable list: names, SELECT positions or widths.
pub(crate) fn comma_list(items: &[impl fmt::Display]) -> String {
    let mut texts = Vec::with_capacity(items.len());
    for item in items {
        texts.push(item.to_string());
    }
    texts.join(", ")
}

/// A table reference's name and alias, as written.
// Borrows both from the tree, which outlives the walk; a copy would clone
// them for nothing.
struct NamedTable<'tree> {
    name: &'tree sql::ObjectName,
    alias: Option<&'tree sql::TableAlias>,
}

/// The parts of a condition joined by AND. A parenthesis is a part of its
/// own, which every caller refuses: AND needs none.
// Borrows the parts from the tree, which outlives the walk.
fn and_parts<'tree>(expression: &'tree sql::Expr, parts: &mut Vec<&'tree sql::Expr>) {
    match expression {
        sql::Expr::BinaryOp {
            left,
            op: sql::BinaryOperator::And,
            right,
        } => {
            and_parts(left, parts);
            and_parts(right, parts);
        }
        other => parts.push(other),
    }
}
