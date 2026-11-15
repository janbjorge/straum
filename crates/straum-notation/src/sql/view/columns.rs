use sqlparser::ast as sql;
use straum_domain::{Attribute, ByKey, Condition, Identifier, Literal, Via};

use super::{KeyKind, KeyRead, Qualified, Reference, ResolvedColumn, Scope, ScopedKey};
use crate::sql::{Walker, line};

impl Walker {
    /// A key item: a fact column, its null-ness, or a dim column through its
    /// ref. Recorded in `scope` too, so a FILTER can reach it.
    pub(super) fn key(
        &mut self,
        scope: &mut Scope,
        expr: &sql::Expr,
        alias: &sql::Ident,
        by: &mut Vec<ByKey>,
        attributes: &mut [Vec<Attribute>],
    ) {
        let name = self.read_identifier(alias);
        if let sql::Expr::IsNull(_) | sql::Expr::IsNotNull(_) = expr {
            self.refuse(
                line(expr),
                &format!("null-ness key without parentheses, write ({expr}),"),
                expr,
            );
            return;
        }
        if let sql::Expr::Nested(_) = expr
            && null_test(expr).is_none()
        {
            self.refuse(
                line(expr),
                "parentheses, write alias.column or (alias.column IS NULL),",
                expr,
            );
            return;
        }
        let spelled = match null_test(expr) {
            Some((inner, _)) => inner,
            None => expr,
        };
        let read = self.key_read(scope, expr);
        let (
            Some(name),
            Some(KeyRead {
                column,
                is_null,
                cast,
            }),
        ) = (name, read)
        else {
            return;
        };
        self.same_spelling(scope, spelled, &column, cast);
        match (&column, is_null) {
            (ResolvedColumn::Fact(fact_column), None) => by.push(ByKey::Column {
                column: fact_column.clone(),
                name: name.clone(),
            }),
            (ResolvedColumn::Fact(fact_column), Some(is_null)) => by.push(ByKey::IsNull {
                column: fact_column.clone(),
                name: name.clone(),
                negated: !is_null,
            }),
            (
                ResolvedColumn::Attribute {
                    through,
                    column: attribute_column,
                },
                _,
            ) => attributes[*through].push(Attribute {
                name: name.clone(),
                column: attribute_column.clone(),
            }),
        }
        scope.keys.push(ScopedKey {
            name,
            column,
            is_null,
            cast,
            spelled: spelling(spelled),
        });
    }

    pub(super) fn key_read(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<KeyRead> {
        if let Some((inner, negated)) = null_test(expr) {
            let (column, cast) = self.fact_column(scope, inner, "the IS NULL column")?;
            return Some(KeyRead {
                column: ResolvedColumn::Fact(column),
                is_null: Some(!negated),
                cast,
            });
        }
        let (column, cast) = self.column(scope, expr)?;
        Some(KeyRead {
            column,
            is_null: None,
            cast,
        })
    }

    /// A column is one spelling throughout a view: with `::text` or
    /// without, as its type decides.
    pub(super) fn same_spelling(
        &mut self,
        scope: &Scope,
        expr: &sql::Expr,
        column: &ResolvedColumn,
        cast: bool,
    ) {
        let other = (scope.keys.iter()).find(|key| key.column == *column && key.cast != cast);
        if let Some(other) = other {
            self.error(
                line(expr),
                format!(
                    "{}: SELECT spells the column {}; write it alike",
                    spelling(expr),
                    other.spelled
                ),
            );
        }
    }

    /// A fact column that can't be `::text`: the cast goes on a key or a
    /// FILTER column, where the text is what Straum keeps.
    pub(super) fn uncast_fact_column(
        &mut self,
        scope: &Scope,
        expr: &sql::Expr,
        what: &str,
    ) -> Option<Identifier> {
        let (column, cast) = self.fact_column(scope, expr, what)?;
        if cast {
            self.error(
                line(expr),
                format!(
                    "{what} {} can't be ::text; it goes on a key or a FILTER column",
                    spelling(expr)
                ),
            );
            return None;
        }
        Some(column)
    }

    pub(crate) fn literal(&mut self, expr: &sql::Expr) -> Option<Literal> {
        let (negative, value) = match expr {
            sql::Expr::UnaryOp {
                op: sql::UnaryOperator::Minus,
                expr: inner,
            } => (true, inner.as_ref()),
            other => (false, other),
        };
        let literal = match value {
            sql::Expr::Value(sql::ValueWithSpan { value, span: _ }) => match value {
                sql::Value::Number(digits, false) => {
                    let digits = if negative {
                        format!("-{digits}")
                    } else {
                        digits.clone()
                    };
                    digits.parse().ok().map(Literal::Int)
                }
                sql::Value::SingleQuotedString(text) if !negative => {
                    Some(Literal::Text(text.clone()))
                }
                sql::Value::Boolean(value) if !negative => Some(Literal::Bool(*value)),
                _ => None,
            },
            _ => None,
        };
        if literal.is_none() {
            self.refuse(
                line(expr),
                "literal, use an integer, 'text', true or false,",
                expr,
            );
        }
        literal
    }

    /// `alias.column`, or `alias.column::text` to read an enum column as
    /// the label PostgreSQL prints (ADR-0037). `true` when cast.
    pub(super) fn reference(&mut self, expr: &sql::Expr) -> Option<Reference> {
        let sql::Expr::Cast {
            kind,
            expr: inner,
            data_type,
            format,
        } = expr
        else {
            let Qualified { alias, column } = self.qualified(expr)?;
            return Some(Reference {
                alias,
                column,
                cast: false,
            });
        };
        let at = line(expr);
        self.absent(at, format.is_some(), "CAST ... FORMAT");
        match kind {
            sql::CastKind::DoubleColon => {}
            sql::CastKind::Cast => {
                self.refuse(at, "CAST, write alias.column::text,", expr);
                return None;
            }
            sql::CastKind::TryCast | sql::CastKind::SafeCast => {
                self.refuse(at, "cast", expr);
                return None;
            }
        }
        if !matches!(data_type, sql::DataType::Text) {
            self.refuse(at, &format!("::{data_type}, write ::text,"), expr);
            return None;
        }
        let Qualified { alias, column } = self.qualified(inner)?;
        Some(Reference {
            alias,
            column,
            cast: true,
        })
    }

    /// `alias.column`, the fact's included: no table is anyone's default.
    pub(super) fn qualified(&mut self, expr: &sql::Expr) -> Option<Qualified> {
        match expr {
            sql::Expr::Cast { .. } => {
                self.refuse(line(expr), "::text outside a key or a FILTER column,", expr);
                None
            }
            sql::Expr::CompoundIdentifier(ids) => {
                let [alias, column] = ids.as_slice() else {
                    self.refuse(line(expr), "column, write alias.column,", expr);
                    return None;
                };
                Some(Qualified {
                    alias: self.read_identifier(alias)?,
                    column: self.read_identifier(column)?,
                })
            }
            other => {
                self.refuse(line(other), "column, write alias.column,", other);
                None
            }
        }
    }

    pub(super) fn column(
        &mut self,
        scope: &Scope,
        expr: &sql::Expr,
    ) -> Option<(ResolvedColumn, bool)> {
        let Reference {
            alias,
            column,
            cast,
        } = self.reference(expr)?;
        if alias == scope.alias {
            return Some((ResolvedColumn::Fact(column), cast));
        }
        let Some(through) = (scope.refs.iter()).position(|joined| joined.reference.alias == alias)
        else {
            self.error(line(expr), format!("unknown table alias {alias}"));
            return None;
        };
        let joined = &scope.refs[through];
        let position = joined.key.iter().position(|key| *key == column);
        let column = match position.map(|position| &joined.reference.via[position]) {
            Some(Via::Fact(via)) => ResolvedColumn::Fact(via.clone()),
            Some(Via::Attribute { .. }) | None => ResolvedColumn::Attribute { through, column },
        };
        Some((column, cast))
    }

    pub(super) fn fact_column(
        &mut self,
        scope: &Scope,
        expr: &sql::Expr,
        what: &str,
    ) -> Option<(Identifier, bool)> {
        match self.column(scope, expr)? {
            (ResolvedColumn::Fact(column), cast) => Some((column, cast)),
            (ResolvedColumn::Attribute { .. }, _) => {
                self.error(
                    line(expr),
                    format!("{what} must be a {} column", scope.alias),
                );
                None
            }
        }
    }

    /// `FILTER (WHERE alias.column = ...)` reaches the key that reads the
    /// column, by the column, as PostgreSQL reads the view: an output name
    /// is no column there.
    pub(crate) fn key_of(&mut self, scope: &Scope, expr: &sql::Expr) -> Option<Identifier> {
        let (column, cast) = self.column(scope, expr)?;
        let Some(key) = self.filter_key(scope, expr, &column, cast, KeyKind::Value) else {
            self.error(
                line(expr),
                format!("FILTER on {expr} needs {expr} AS <name> in SELECT"),
            );
            return None;
        };
        Some(key.name.clone())
    }

    /// `FILTER (WHERE alias.column IS NULL)` counts through the null-ness
    /// key over that column, which holds for the rows the test picks out.
    pub(crate) fn null_condition(
        &mut self,
        scope: &Scope,
        test: &sql::Expr,
        expr: &sql::Expr,
        is_null: bool,
    ) -> Option<Condition> {
        let (column, cast) = self.fact_column(scope, expr, "the IS NULL column")?;
        let fact_column = ResolvedColumn::Fact(column);
        let Some(key) = self.filter_key(scope, expr, &fact_column, cast, KeyKind::NullNess) else {
            self.error(
                line(test),
                format!("FILTER (WHERE {test}) needs the key ({expr} IS NULL) AS <name> in SELECT"),
            );
            return None;
        };
        Some(Condition {
            key: key.name.clone(),
            values: vec![Literal::Bool(
                is_null == key.is_null.expect("a null-ness key"),
            )],
        })
    }

    /// The key over a FILTER's column, a null-ness key or not, spelled as
    /// SELECT spells it; `None` when SELECT has none, which the caller
    /// reports in its own words.
    pub(super) fn filter_key<'s>(
        &mut self,
        scope: &'s Scope,
        expr: &sql::Expr,
        column: &ResolvedColumn,
        cast: bool,
        kind: KeyKind,
    ) -> Option<&'s ScopedKey> {
        let null_ness = matches!(kind, KeyKind::NullNess);
        let key = (scope.keys.iter())
            .find(|key| key.column == *column && key.is_null.is_some() == null_ness)?;
        self.same_spelling(scope, expr, column, cast);
        Some(key)
    }
}

/// `(column IS NULL)` or `(column IS NOT NULL)`, in its one pair of
/// parentheses, and whether it is the `NOT` one.
pub(super) fn null_test(expr: &sql::Expr) -> Option<(&sql::Expr, bool)> {
    let sql::Expr::Nested(inner) = expr else {
        return None;
    };
    match inner.as_ref() {
        sql::Expr::IsNull(column) => Some((column, false)),
        sql::Expr::IsNotNull(column) => Some((column, true)),
        _ => None,
    }
}

/// A column reference as Straum SQL spells it: `sqlparser` prints the cast
/// as `::TEXT`.
pub(super) fn spelling(expr: &sql::Expr) -> String {
    match expr {
        sql::Expr::Cast { expr: inner, .. } => format!("{inner}::text"),
        other => other.to_string(),
    }
}
