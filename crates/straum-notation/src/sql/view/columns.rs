//! The columns a view names, and the keys that read them. A column is
//! written `alias.column`, maybe `::text`; it resolves to a fact column or
//! to a dim column through its join. A key item records what it reads in
//! the scope, so a FILTER or the view's WHERE can reach the key by its
//! column, as PostgreSQL reads the view. Literals are read here too.

use sqlparser::ast as sql;
use straum_domain::{Attribute, ByKey, Condition, Identifier, Literal, RefKind, Via};

use super::joins::{key_position, ref_position};
use super::{Cast, KeyForm, Qualified, ResolvedColumn, Scope, ScopedKey, SelectedKeys};
use crate::sql::{Clause, NullTest, Reported, Walker, line};

/// `alias.column` and whether it is cast, before the alias is resolved.
struct Reference {
    alias: Identifier,
    column: Identifier,
    cast: Cast,
}

/// What a key item reads: its column, the value or its null-ness, and
/// how it is spelled.
struct KeyRead {
    column: ResolvedColumn,
    form: KeyForm,
    cast: Cast,
}

/// A column reference, resolved.
struct ColumnRead {
    column: ResolvedColumn,
    cast: Cast,
}

/// A column reference that must name a fact column, resolved.
struct FactColumnRead {
    column: Identifier,
    cast: Cast,
}

/// Which keys a FILTER's column may reach: value keys, or null-ness keys.
#[derive(Clone, Copy)]
enum KeyKind {
    Value,
    NullNess,
}

/// The key a FILTER's column reaches.
struct FilterKey {
    name: Identifier,
    form: KeyForm,
}

/// `(column IS NULL)` or `(column IS NOT NULL)`, as written.
// Borrows the column from the tree, which outlives the walk.
struct NullTestOf<'tree> {
    column: &'tree sql::Expr,
    test: NullTest,
}

impl Walker {
    /// A key item: a fact column, its null-ness, or a dim column through its
    /// ref. Recorded in `scope` too, so a FILTER can reach it.
    pub(super) fn key(
        &mut self,
        scope: &mut Scope,
        expression: &sql::Expr,
        alias: &sql::Ident,
        selected: &mut SelectedKeys,
    ) {
        let name = self.read_identifier(alias);
        if let sql::Expr::IsNull(_) | sql::Expr::IsNotNull(_) = expression {
            self.refuse(
                line(expression),
                &format!("null-ness key without parentheses, write ({expression}),"),
                expression,
            );
            return;
        }
        let null_test = null_test(expression);
        if let sql::Expr::Nested(_) = expression
            && null_test.is_none()
        {
            self.refuse(
                line(expression),
                "parentheses, write alias.column or (alias.column IS NULL),",
                expression,
            );
            return;
        }
        let written = match &null_test {
            Some(null_test) => null_test.column,
            None => expression,
        };
        let read = self.key_read(scope, expression);
        let Ok(name) = name else {
            return;
        };
        let Ok(KeyRead { column, form, cast }) = read else {
            return;
        };
        self.same_spelling(scope, written, &column, cast);
        record_key(selected, &name, &column, form);
        scope.keys.push(ScopedKey {
            name,
            column,
            form,
            cast,
            written: spelling(written),
        });
    }

    fn key_read(&mut self, scope: &Scope, expression: &sql::Expr) -> Result<KeyRead, Reported> {
        if let Some(NullTestOf { column, test }) = null_test(expression) {
            let FactColumnRead { column, cast } =
                self.fact_column(scope, column, "the IS NULL column")?;
            return Ok(KeyRead {
                column: ResolvedColumn::Fact(column),
                form: KeyForm::NullNess(test),
                cast,
            });
        }
        let ColumnRead { column, cast } = self.column(scope, expression)?;
        Ok(KeyRead {
            column,
            form: KeyForm::Value,
            cast,
        })
    }

    /// A column is one spelling throughout a view: with `::text` or
    /// without, as its type decides.
    fn same_spelling(
        &mut self,
        scope: &Scope,
        expression: &sql::Expr,
        column: &ResolvedColumn,
        cast: Cast,
    ) {
        for other in &scope.keys {
            if other.column == *column && other.cast != cast {
                self.error(
                    line(expression),
                    format!(
                        "{}: SELECT spells the column {}; write it alike",
                        spelling(expression),
                        other.written
                    ),
                );
                return;
            }
        }
    }

    /// A fact column that can't be `::text`: the cast goes on a key or a
    /// FILTER column, where the text is what Straum keeps.
    pub(super) fn uncast_fact_column(
        &mut self,
        scope: &Scope,
        expression: &sql::Expr,
        what: &str,
    ) -> Result<Identifier, Reported> {
        let FactColumnRead { column, cast } = self.fact_column(scope, expression, what)?;
        if cast == Cast::AsText {
            self.error(
                line(expression),
                format!(
                    "{what} {} can't be ::text; it goes on a key or a FILTER column",
                    spelling(expression)
                ),
            );
            return Err(Reported);
        }
        Ok(column)
    }

    /// An integer, `'text'`, `true` or `false`, as a view's condition
    /// writes it.
    pub(crate) fn literal(&mut self, expression: &sql::Expr) -> Result<Literal, Reported> {
        let literal = match expression {
            sql::Expr::UnaryOp {
                op: sql::UnaryOperator::Minus,
                expr: inner,
            } => negative_literal(inner),
            other => plain_literal(other),
        };
        let Some(literal) = literal else {
            self.refuse(
                line(expression),
                "literal, use an integer, 'text', true or false,",
                expression,
            );
            return Err(Reported);
        };
        Ok(literal)
    }

    /// `alias.column`, or `alias.column::text` to read an enum column as
    /// the label PostgreSQL prints (ADR-0004).
    fn reference(&mut self, expression: &sql::Expr) -> Result<Reference, Reported> {
        let sql::Expr::Cast {
            kind,
            expr: inner,
            data_type,
            format,
        } = expression
        else {
            let Qualified { alias, column } = self.qualified(expression)?;
            return Ok(Reference {
                alias,
                column,
                cast: Cast::Plain,
            });
        };
        let at = line(expression);
        self.reject_if_present(at, format.is_some(), "CAST ... FORMAT");
        match kind {
            sql::CastKind::DoubleColon => {}
            sql::CastKind::Cast => {
                self.refuse(at, "CAST, write alias.column::text,", expression);
                return Err(Reported);
            }
            sql::CastKind::TryCast | sql::CastKind::SafeCast => {
                self.refuse(at, "cast", expression);
                return Err(Reported);
            }
        }
        if !matches!(data_type, sql::DataType::Text) {
            self.refuse(at, &format!("::{data_type}, write ::text,"), expression);
            return Err(Reported);
        }
        let Qualified { alias, column } = self.qualified(inner)?;
        Ok(Reference {
            alias,
            column,
            cast: Cast::AsText,
        })
    }

    /// `alias.column`, the fact's included: no table is anyone's default.
    pub(super) fn qualified(&mut self, expression: &sql::Expr) -> Result<Qualified, Reported> {
        match expression {
            sql::Expr::Cast { .. } => {
                self.refuse(
                    line(expression),
                    "::text outside a key or a FILTER column,",
                    expression,
                );
                Err(Reported)
            }
            sql::Expr::CompoundIdentifier(identifiers) => {
                let [alias, column] = identifiers.as_slice() else {
                    self.refuse(line(expression), "column, write alias.column,", expression);
                    return Err(Reported);
                };
                Ok(Qualified {
                    alias: self.read_identifier(alias)?,
                    column: self.read_identifier(column)?,
                })
            }
            other => {
                self.refuse(line(other), "column, write alias.column,", other);
                Err(Reported)
            }
        }
    }

    fn column(&mut self, scope: &Scope, expression: &sql::Expr) -> Result<ColumnRead, Reported> {
        let Reference {
            alias,
            column,
            cast,
        } = self.reference(expression)?;
        if alias == scope.alias {
            return Ok(ColumnRead {
                column: ResolvedColumn::Fact(column),
                cast,
            });
        }
        let Some(through) = ref_position(scope, &alias) else {
            self.error(line(expression), format!("unknown table alias {alias}"));
            return Err(Reported);
        };
        let joined = &scope.refs[through];
        let kind = joined.reference.kind;
        if let RefKind::Semi | RefKind::Anti = kind {
            self.error(
                line(expression),
                format!(
                    "{alias}.{column} is read outside its EXISTS; {alias} is {kind}, \
                     which keeps no row to read"
                ),
            );
            return Err(Reported);
        }
        // Under INNER JOIN a dim's key column reads the fact column it was
        // joined on. Under LEFT OUTER JOIN it is null where no dim row
        // matches, and the fact column is not, so it is refused.
        if let Some(position) = key_position(joined, &column)
            && let Via::Fact(via) = &joined.reference.via[position]
        {
            if joined.reference.kind == RefKind::Left {
                self.error(
                    line(expression),
                    left_key_message(scope, &alias, &column, via),
                );
                return Err(Reported);
            }
            return Ok(ColumnRead {
                column: ResolvedColumn::Fact(via.clone()),
                cast,
            });
        }
        Ok(ColumnRead {
            column: ResolvedColumn::Attribute { through, column },
            cast,
        })
    }

    fn fact_column(
        &mut self,
        scope: &Scope,
        expression: &sql::Expr,
        what: &str,
    ) -> Result<FactColumnRead, Reported> {
        let ColumnRead { column, cast } = self.column(scope, expression)?;
        let ResolvedColumn::Fact(column) = column else {
            self.error(
                line(expression),
                format!("{what} must be a {} column", scope.alias),
            );
            return Err(Reported);
        };
        Ok(FactColumnRead { column, cast })
    }

    /// `alias.column = ...` in a `WHERE` or `FILTER (WHERE ..)` reaches
    /// the key that reads the column, by the column, as PostgreSQL reads
    /// the view: an output name is no column there.
    pub(crate) fn key_of(
        &mut self,
        scope: &Scope,
        expression: &sql::Expr,
        clause: Clause,
    ) -> Result<Identifier, Reported> {
        let ColumnRead { column, cast } = self.column(scope, expression)?;
        let Some(key) = filter_key(scope, &column, KeyKind::Value) else {
            let clause = match clause {
                Clause::Where => "WHERE",
                Clause::Filter => "FILTER",
            };
            self.error(
                line(expression),
                format!("{clause} on {expression} needs {expression} AS <name> in SELECT"),
            );
            return Err(Reported);
        };
        self.same_spelling(scope, expression, &column, cast);
        Ok(key.name)
    }

    /// `alias.column IS NULL` in a `WHERE` or `FILTER (WHERE ..)` counts
    /// through the null-ness key over that column, which holds for the
    /// rows the test picks out.
    pub(crate) fn null_condition(
        &mut self,
        scope: &Scope,
        clause: Clause,
        whole_test: &sql::Expr,
        expression: &sql::Expr,
        test: NullTest,
    ) -> Result<Condition, Reported> {
        let FactColumnRead { column, cast } =
            self.fact_column(scope, expression, "the IS NULL column")?;
        let column = ResolvedColumn::Fact(column);
        let Some(key) = filter_key(scope, &column, KeyKind::NullNess) else {
            let written = match clause {
                Clause::Where => format!("WHERE {whole_test}"),
                Clause::Filter => format!("FILTER (WHERE {whole_test})"),
            };
            self.error(
                line(whole_test),
                format!("{written} needs the key ({expression} IS NULL) AS <name> in SELECT"),
            );
            return Err(Reported);
        };
        self.same_spelling(scope, expression, &column, cast);
        Ok(Condition {
            key: key.name,
            values: vec![Literal::Bool(key.form == KeyForm::NullNess(test))],
        })
    }
}

/// Adds a key item to the view's keys: a fact column or its null-ness to
/// the rollup keys, a dim column to its ref's reads.
fn record_key(
    selected: &mut SelectedKeys,
    name: &Identifier,
    column: &ResolvedColumn,
    form: KeyForm,
) {
    match column {
        ResolvedColumn::Fact(fact_column) => {
            let by_key = match form {
                KeyForm::Value => ByKey::Column {
                    column: fact_column.clone(),
                    name: name.clone(),
                },
                KeyForm::NullNess(test) => ByKey::IsNull {
                    column: fact_column.clone(),
                    name: name.clone(),
                    negated: test == NullTest::IsNotNull,
                },
            };
            selected.by_keys.push(by_key);
        }
        ResolvedColumn::Attribute { through, column } => {
            selected.attributes[*through].push(Attribute {
                name: name.clone(),
                column: column.clone(),
            });
        }
    }
}

/// Why a key column of a `LEFT OUTER JOIN` can't be read, and what to
/// write instead.
pub(super) fn left_key_message(
    scope: &Scope,
    alias: &Identifier,
    column: &Identifier,
    via: &Identifier,
) -> String {
    let fact = &scope.alias;
    format!(
        "{alias}.{column} is null where no {alias} row matches, and {fact}.{via} is not; \
         write {fact}.{via}, or join {alias} with INNER JOIN"
    )
}

/// The key over a FILTER's column, a null-ness key or not; `None` when
/// SELECT has none, which the caller reports in its own words.
fn filter_key(scope: &Scope, column: &ResolvedColumn, kind: KeyKind) -> Option<FilterKey> {
    let wants_null_ness = matches!(kind, KeyKind::NullNess);
    for key in &scope.keys {
        let reads_null_ness = matches!(key.form, KeyForm::NullNess(_));
        if key.column == *column && reads_null_ness == wants_null_ness {
            return Some(FilterKey {
                name: key.name.clone(),
                form: key.form,
            });
        }
    }
    None
}

/// `(column IS NULL)` or `(column IS NOT NULL)`, in its one pair of
/// parentheses.
fn null_test(expression: &sql::Expr) -> Option<NullTestOf<'_>> {
    let sql::Expr::Nested(inner) = expression else {
        return None;
    };
    match inner.as_ref() {
        sql::Expr::IsNull(column) => Some(NullTestOf {
            column,
            test: NullTest::IsNull,
        }),
        sql::Expr::IsNotNull(column) => Some(NullTestOf {
            column,
            test: NullTest::IsNotNull,
        }),
        _ => None,
    }
}

/// An integer, `'text'`, `true` or `false`, with no sign before it.
fn plain_literal(expression: &sql::Expr) -> Option<Literal> {
    let sql::Expr::Value(sql::ValueWithSpan { value, span: _ }) = expression else {
        return None;
    };
    match value {
        sql::Value::Number(digits, false) => Some(Literal::Int(digits.parse().ok()?)),
        sql::Value::SingleQuotedString(text) => Some(Literal::Text(text.clone())),
        sql::Value::Boolean(value) => Some(Literal::Bool(*value)),
        _ => None,
    }
}

/// `-` and an integer; nothing else takes a sign.
fn negative_literal(expression: &sql::Expr) -> Option<Literal> {
    let sql::Expr::Value(sql::ValueWithSpan {
        value: sql::Value::Number(digits, false),
        span: _,
    }) = expression
    else {
        return None;
    };
    Some(Literal::Int(format!("-{digits}").parse().ok()?))
}

/// A column reference as Straum SQL spells it: `sqlparser` prints the cast
/// as `::TEXT`.
fn spelling(expression: &sql::Expr) -> String {
    match expression {
        sql::Expr::Cast { expr: inner, .. } => format!("{inner}::text"),
        other => other.to_string(),
    }
}
