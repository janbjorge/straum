//! Reads a view's FROM: the fact table, its `LEFT OUTER` and `INNER`
//! joins, and the `EXISTS` and `NOT EXISTS` in WHERE, which are semi and
//! anti joins. Each join becomes a `JoinedRef` in the view's scope; the
//! rest of WHERE is handed back, to be read once the keys are known.

use sqlparser::ast as sql;
use straum_domain::{Identifier, Ref, RefKind, Table, Via};

use super::{JoinedRef, Qualified, Scope};
use crate::sql::{NamedTable, Reported, Walker, and_parts, line};

/// `schema.table AS alias`, as read.
struct AliasedTable {
    table: Table,
    alias: Identifier,
}

/// A join's key columns and the via columns they meet, pair by pair.
struct Equalities {
    key: Vec<Identifier>,
    via: Vec<Via>,
}

/// One `alias.key = near.column` of a join.
struct Equality {
    key: Identifier,
    via: Via,
}

/// FROM read: the fact table, the scope its joins make, and the WHERE
/// conditions left for later.
// Borrows the conditions from the tree, which outlives the walk.
pub(super) struct FromClause<'tree> {
    pub(super) table: Table,
    pub(super) scope: Scope,
    pub(super) conditions: Vec<&'tree sql::Expr>,
}

impl Walker {
    /// The fact, its joins, and the semi and anti joins in WHERE; the rest
    /// of WHERE waits for the keys, which it names.
    pub(super) fn from<'tree>(
        &mut self,
        from: &sql::TableWithJoins,
        selection: Option<&'tree sql::Expr>,
    ) -> Result<FromClause<'tree>, Reported> {
        let sql::TableWithJoins { relation, joins } = from;
        let AliasedTable { table, alias } = self.table(relation)?;
        let mut scope = Scope {
            alias,
            refs: Vec::new(),
            keys: Vec::new(),
        };
        for join in joins {
            let Ok(joined) = self.join(&scope, join) else {
                continue;
            };
            let alias = &joined.reference.alias;
            if alias_taken(&scope, alias) {
                self.error(line(join), format!("alias {alias} used twice"));
                continue;
            }
            scope.refs.push(joined);
        }
        let mut parts = Vec::new();
        if let Some(selection) = selection {
            and_parts(selection, &mut parts);
        }
        let mut conditions = Vec::new();
        for part in parts {
            let sql::Expr::Exists { subquery, negated } = part else {
                conditions.push(part);
                continue;
            };
            let kind = if *negated {
                RefKind::Anti
            } else {
                RefKind::Semi
            };
            let Ok(joined) = self.exists(&scope, part, subquery, kind) else {
                continue;
            };
            let alias = &joined.reference.alias;
            if alias_taken(&scope, alias) {
                self.error(line(part), format!("alias {alias} used twice"));
                continue;
            }
            scope.refs.push(joined);
        }
        Ok(FromClause {
            table,
            scope,
            conditions,
        })
    }

    /// `EXISTS (SELECT 1 FROM schema.dim AS alias WHERE alias.key =
    /// near.column AND ...)`, or `NOT EXISTS`: a semi or anti join, which
    /// keeps or drops the fact row and reads nothing of the dim.
    fn exists(
        &mut self,
        scope: &Scope,
        expression: &sql::Expr,
        subquery: &sql::Query,
        kind: RefKind,
    ) -> Result<JoinedRef, Reported> {
        let at = line(expression);
        let select = self.one_select(subquery, "EXISTS")?;
        self.select_clauses(select);
        if !selects_one(&select.projection) {
            self.refuse(at, "EXISTS, write EXISTS (SELECT 1 FROM ...),", expression);
            return Err(Reported);
        }
        if !groups_by_nothing(&select.group_by) {
            self.not_in_subset(at, "GROUP BY inside EXISTS");
        }
        let [from] = select.from.as_slice() else {
            self.refuse(
                at,
                "EXISTS, write EXISTS (SELECT 1 FROM one table),",
                expression,
            );
            return Err(Reported);
        };
        if !from.joins.is_empty() {
            self.not_in_subset(at, "a join inside EXISTS");
        }
        let table = self.table(&from.relation)?;
        let Some(on) = &select.selection else {
            self.refuse(at, "EXISTS without WHERE alias.key = ...,", expression);
            return Err(Reported);
        };
        self.joined(scope, kind, table, on)
    }

    fn joined(
        &mut self,
        scope: &Scope,
        kind: RefKind,
        aliased: AliasedTable,
        on: &sql::Expr,
    ) -> Result<JoinedRef, Reported> {
        let AliasedTable { table, alias } = aliased;
        let Equalities { key, via } = self.equalities(scope, &alias, on)?;
        Ok(JoinedRef {
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

    /// The key equalities of a join: `alias.key = near.column` joined by
    /// `AND`, the joined table's column on the left, the other the fact's
    /// or a column of a dim joined before.
    fn equalities(
        &mut self,
        scope: &Scope,
        alias: &Identifier,
        on: &sql::Expr,
    ) -> Result<Equalities, Reported> {
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
                return Err(Reported);
            };
            key.push(equality.key);
            via.push(equality.via);
        }
        Ok(Equalities { key, via })
    }

    /// One key equality; `None` when the part is none, which the caller
    /// reports for the whole ON.
    fn equality(
        &mut self,
        scope: &Scope,
        alias: &Identifier,
        expression: &sql::Expr,
    ) -> Option<Equality> {
        let sql::Expr::BinaryOp {
            left,
            op: sql::BinaryOperator::Eq,
            right,
        } = expression
        else {
            return None;
        };
        // Both sides are walked before either is judged, so each reports
        // its own errors.
        let joined_side = self.qualified(left);
        let near_side = self.qualified(right);
        let Ok(joined_side) = joined_side else {
            return None;
        };
        let Ok(near_side) = near_side else {
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

    /// `schema.table AS alias`, every part written: nothing defaults to
    /// `public` or to the table's name.
    fn table(&mut self, factor: &sql::TableFactor) -> Result<AliasedTable, Reported> {
        let NamedTable { name, alias } = self.table_factor(factor)?;
        let at = line(factor);
        let table = self.schema_table(name);
        let Some(sql::TableAlias {
            explicit: true,
            name: alias,
            columns,
            at: at_time,
        }) = alias
        else {
            self.error(
                line(name),
                format!("table {name} must be {name} AS <alias>"),
            );
            return Err(Reported);
        };
        if !columns.is_empty() {
            self.not_in_subset(at, "an alias column list");
        }
        if at_time.is_some() {
            self.not_in_subset(at, "AT");
        }
        let alias = self.read_identifier(alias)?;
        Ok(AliasedTable {
            table: table?,
            alias,
        })
    }

    /// `LEFT OUTER JOIN schema.dim AS alias ON alias.key = near.column` or
    /// `INNER JOIN ...`, spelled whole, with more such equalities joined by
    /// `AND` for a key of several columns, the dim's column on the left of
    /// each; `near` is the fact or a dim joined before.
    fn join(&mut self, scope: &Scope, join: &sql::Join) -> Result<JoinedRef, Reported> {
        let sql::Join {
            relation,
            global,
            join_operator,
        } = join;
        let at = line(join);
        if *global {
            self.not_in_subset(at, "GLOBAL");
        }
        match join_operator {
            sql::JoinOperator::LeftOuter(constraint) => {
                self.join_on(scope, join, relation, RefKind::Left, constraint)
            }
            sql::JoinOperator::Inner(constraint) => {
                self.join_on(scope, join, relation, RefKind::Inner, constraint)
            }
            sql::JoinOperator::Left(_) => {
                self.refuse(at, "LEFT JOIN, write LEFT OUTER JOIN,", join);
                Err(Reported)
            }
            sql::JoinOperator::Join(_) => {
                self.refuse(at, "JOIN, write INNER JOIN,", join);
                Err(Reported)
            }
            _ => {
                self.refuse(at, "join, write INNER JOIN or LEFT OUTER JOIN,", join);
                Err(Reported)
            }
        }
    }

    /// A join of a kind Straum SQL takes: its table, then its ON.
    fn join_on(
        &mut self,
        scope: &Scope,
        join: &sql::Join,
        relation: &sql::TableFactor,
        kind: RefKind,
        constraint: &sql::JoinConstraint,
    ) -> Result<JoinedRef, Reported> {
        let table = self.table(relation)?;
        let sql::JoinConstraint::On(on) = constraint else {
            self.refuse(line(join), "join condition, write ON,", join);
            return Err(Reported);
        };
        self.joined(scope, kind, table, on)
    }

    fn schema_table(&mut self, name: &sql::ObjectName) -> Result<Table, Reported> {
        let sql::ObjectName(parts) = name;
        let [schema, table] = parts.as_slice() else {
            self.error(line(name), format!("table {name} must be schema.table"));
            return Err(Reported);
        };
        let schema = self.name_part(name, schema)?;
        let table = self.name_part(name, table)?;
        Ok(Table {
            schema,
            name: table,
        })
    }
}

/// `SELECT 1`, the one list an `EXISTS` takes.
fn selects_one(projection: &[sql::SelectItem]) -> bool {
    let [
        sql::SelectItem::UnnamedExpr(sql::Expr::Value(sql::ValueWithSpan {
            value: sql::Value::Number(number, false),
            span: _,
        })),
    ] = projection
    else {
        return false;
    };
    number == "1"
}

fn groups_by_nothing(group_by: &sql::GroupByExpr) -> bool {
    match group_by {
        sql::GroupByExpr::Expressions(expressions, _) => expressions.is_empty(),
        sql::GroupByExpr::All(_) => false,
    }
}

fn alias_taken(scope: &Scope, alias: &Identifier) -> bool {
    if *alias == scope.alias {
        return true;
    }
    for joined in &scope.refs {
        if joined.reference.alias == *alias {
            return true;
        }
    }
    false
}

/// The place in `scope.refs` of the join under `alias`.
pub(super) fn ref_position(scope: &Scope, alias: &Identifier) -> Option<usize> {
    for (position, joined) in scope.refs.iter().enumerate() {
        if joined.reference.alias == *alias {
            return Some(position);
        }
    }
    None
}

/// The place of `column` in the join's key.
pub(super) fn key_position(joined: &JoinedRef, column: &Identifier) -> Option<usize> {
    for (position, key) in joined.key.iter().enumerate() {
        if key == column {
            return Some(position);
        }
    }
    None
}

/// The near side of a join equality: a fact column, or a column of a
/// dim joined before, which then carries it.
fn near(scope: &Scope, near_side: Qualified) -> Option<Via> {
    let Qualified { alias, column } = near_side;
    if alias == scope.alias {
        return Some(Via::Fact(column));
    }
    let through = ref_position(scope, &alias)?;
    let joined = &scope.refs[through];
    match joined.reference.kind {
        RefKind::Left | RefKind::Inner => {}
        RefKind::Semi | RefKind::Anti => return None,
    }
    // A key column of that dim is the column it was joined on.
    if let Some(position) = key_position(joined, &column)
        && let Via::Fact(_) = &joined.reference.via[position]
    {
        return Some(joined.reference.via[position].clone());
    }
    Some(Via::Attribute { through, column })
}
