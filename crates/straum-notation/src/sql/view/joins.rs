use sqlparser::ast as sql;
use straum_domain::{Identifier, Ref, RefKind, Table, Via};

use super::{AliasedTable, Equalities, Equality, FromClause, JoinedRef, Qualified, Scope};
use crate::sql::{Walker, and_parts, line};

impl Walker {
    /// `EXISTS (SELECT 1 FROM schema.dim AS alias WHERE alias.key =
    /// near.column AND ...)`, or `NOT EXISTS`: a semi or anti join, which
    /// keeps or drops the fact row and reads nothing of the dim.
    pub(super) fn exists(
        &mut self,
        scope: &Scope,
        expr: &sql::Expr,
        subquery: &sql::Query,
        negated: bool,
    ) -> Option<JoinedRef> {
        let at = line(expr);
        let kind = if negated {
            RefKind::Anti
        } else {
            RefKind::Semi
        };
        let select = self.one_select(subquery, "EXISTS")?;
        self.select_clauses(select);
        let one = matches!(
            select.projection.as_slice(),
            [sql::SelectItem::UnnamedExpr(sql::Expr::Value(sql::ValueWithSpan {
                value: sql::Value::Number(number, false),
                ..
            }))] if number == "1"
        );
        if !one {
            self.refuse(at, "EXISTS, write EXISTS (SELECT 1 FROM ...),", expr);
            return None;
        }
        self.absent(
            at,
            !matches!(select.group_by, sql::GroupByExpr::Expressions(ref exprs, _) if exprs.is_empty()),
            "GROUP BY inside EXISTS",
        );
        let [from] = select.from.as_slice() else {
            self.refuse(at, "EXISTS, write EXISTS (SELECT 1 FROM one table),", expr);
            return None;
        };
        self.absent(at, !from.joins.is_empty(), "a join inside EXISTS");
        let table = self.table(&from.relation)?;
        let Some(on) = &select.selection else {
            self.refuse(at, "EXISTS without WHERE alias.key = ...,", expr);
            return None;
        };
        self.joined(scope, kind, table, on)
    }

    pub(super) fn joined(
        &mut self,
        scope: &Scope,
        kind: RefKind,
        AliasedTable { table, alias }: AliasedTable,
        on: &sql::Expr,
    ) -> Option<JoinedRef> {
        let Equalities { key, via } = self.equalities(scope, &alias, on)?;
        Some(JoinedRef {
            reference: Ref {
                alias,
                dim: table.name.clone(),
                kind,
                via,
                reads: Vec::new(),
            },
            table,
            key,
            as_text: Vec::new(),
        })
    }

    pub(super) fn push_ref(&mut self, scope: &mut Scope, at: u64, joined: JoinedRef) {
        let name = &joined.reference.alias;
        if *name == scope.alias || (scope.refs.iter()).any(|other| other.reference.alias == *name) {
            self.error(at, format!("alias {name} used twice"));
            return;
        }
        scope.refs.push(joined);
    }

    /// The key equalities of a join: `alias.key = near.column` joined by
    /// `AND`, the joined table's column on the left, the other the fact's
    /// or a column of a dim joined before.
    pub(super) fn equalities(
        &mut self,
        scope: &Scope,
        alias: &Identifier,
        on: &sql::Expr,
    ) -> Option<Equalities> {
        let mut parts = Vec::new();
        and_parts(on, &mut parts);
        let mut key = Vec::new();
        let mut via = Vec::new();
        for part in parts {
            let Some(equality) = self.equality(scope, alias, part) else {
                self.error(
                    line(on),
                    format!(
                        "join ON {alias}.<key> = {}.<column>, the joined table's column on \
                         the left, one or more joined by AND; found {on}",
                        scope.alias
                    ),
                );
                return None;
            };
            key.push(equality.key);
            via.push(equality.via);
        }
        Some(Equalities { key, via })
    }

    pub(super) fn equality(
        &mut self,
        scope: &Scope,
        alias: &Identifier,
        expr: &sql::Expr,
    ) -> Option<Equality> {
        let sql::Expr::BinaryOp {
            left,
            op: sql::BinaryOperator::Eq,
            right,
        } = expr
        else {
            return None;
        };
        // Both sides are walked before either is judged, so each reports
        // its own errors.
        let joined_side = self.qualified(left);
        let near_side = self.qualified(right);
        let (Some(joined_side), Some(near_side)) = (joined_side, near_side) else {
            return None;
        };
        if joined_side.alias != *alias {
            return None;
        }
        Some(Equality {
            key: joined_side.column,
            via: near(scope, near_side)?,
        })
    }

    /// The fact, its joins, and the semi and anti joins in WHERE; the rest
    /// of WHERE waits for the keys, which it names.
    pub(super) fn from<'a>(
        &mut self,
        from: &sql::TableWithJoins,
        selection: Option<&'a sql::Expr>,
    ) -> Option<FromClause<'a>> {
        let sql::TableWithJoins { relation, joins } = from;
        let AliasedTable { table, alias } = self.table(relation)?;
        let mut scope = Scope {
            alias,
            refs: Vec::new(),
            keys: Vec::new(),
        };
        for join in joins {
            if let Some(joined) = self.join(&scope, join) {
                self.push_ref(&mut scope, line(join), joined);
            }
        }
        let mut parts = Vec::new();
        if let Some(selection) = selection {
            and_parts(selection, &mut parts);
        }
        let mut conditions = Vec::new();
        for part in parts {
            match part {
                sql::Expr::Exists { subquery, negated } => {
                    if let Some(joined) = self.exists(&scope, part, subquery, *negated) {
                        self.push_ref(&mut scope, line(part), joined);
                    }
                }
                other => conditions.push(other),
            }
        }
        Some(FromClause {
            table,
            scope,
            conditions,
        })
    }

    /// `schema.table AS alias`, every part written: nothing defaults to
    /// `public` or to the table's name.
    pub(super) fn table(&mut self, factor: &sql::TableFactor) -> Option<AliasedTable> {
        let (name, alias) = self.table_factor(factor)?;
        let at = line(factor);
        let table = self.schema_table(name);
        let alias = if let Some(sql::TableAlias {
            explicit: true,
            name: alias,
            columns,
            at: at_time,
        }) = alias
        {
            self.absent(at, !columns.is_empty(), "an alias column list");
            self.absent(at, at_time.is_some(), "AT");
            Some(self.read_identifier(alias)?)
        } else {
            self.error(
                line(name),
                format!("table {name} must be {name} AS <alias>"),
            );
            None
        };
        Some(AliasedTable {
            table: table?,
            alias: alias?,
        })
    }

    /// `LEFT OUTER JOIN schema.dim AS alias ON alias.key = near.column` or
    /// `INNER JOIN ...`, spelled whole, with more such equalities joined by
    /// `AND` for a key of several columns, the dim's column on the left of
    /// each; `near` is the fact or a dim joined before.
    pub(super) fn join(&mut self, scope: &Scope, join: &sql::Join) -> Option<JoinedRef> {
        let sql::Join {
            relation,
            global,
            join_operator,
        } = join;
        let at = line(join);
        self.absent(at, *global, "GLOBAL");
        let (kind, constraint) = match join_operator {
            sql::JoinOperator::LeftOuter(constraint) => (RefKind::Left, constraint),
            sql::JoinOperator::Inner(constraint) => (RefKind::Inner, constraint),
            sql::JoinOperator::Left(_) => {
                self.refuse(at, "LEFT JOIN, write LEFT OUTER JOIN,", join);
                return None;
            }
            sql::JoinOperator::Join(_) => {
                self.refuse(at, "JOIN, write INNER JOIN,", join);
                return None;
            }
            _ => {
                self.refuse(at, "join, write INNER JOIN or LEFT OUTER JOIN,", join);
                return None;
            }
        };
        let table = self.table(relation)?;
        let sql::JoinConstraint::On(on) = constraint else {
            self.refuse(at, "join condition, write ON,", join);
            return None;
        };
        self.joined(scope, kind, table, on)
    }

    pub(super) fn schema_table(&mut self, name: &sql::ObjectName) -> Option<Table> {
        let sql::ObjectName(parts) = name;
        let [schema, table] = parts.as_slice() else {
            self.error(line(name), format!("table {name} must be schema.table"));
            return None;
        };
        let schema = self.name_part(name, schema)?;
        let table = self.name_part(name, table)?;
        Some(Table {
            schema,
            name: table,
        })
    }
}

/// The near side of a join equality: a fact column, or a column of a
/// dim joined before, which then carries it.
pub(super) fn near(scope: &Scope, Qualified { alias, column }: Qualified) -> Option<Via> {
    if alias == scope.alias {
        return Some(Via::Fact(column));
    }
    let through = (scope.refs.iter()).position(|joined| joined.reference.alias == alias)?;
    let joined = &scope.refs[through];
    match joined.reference.kind {
        RefKind::Left | RefKind::Inner => {}
        RefKind::Semi | RefKind::Anti => return None,
    }
    // A key column of that dim is the column it was joined on.
    if let Some(position) = joined.key.iter().position(|key| *key == column)
        && let Via::Fact(_) = &joined.reference.via[position]
    {
        return Some(joined.reference.via[position].clone());
    }
    Some(Via::Attribute { through, column })
}
