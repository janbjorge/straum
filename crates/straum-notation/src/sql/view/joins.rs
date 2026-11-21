//! Reads a view's FROM: the fact table, its `LEFT OUTER` and `INNER`
//! joins, and the `EXISTS` and `NOT EXISTS` in WHERE, which are semi and
//! anti joins. Each join becomes a `JoinedRef` in the view's scope; the
//! rest of WHERE is handed back, to be read once the keys are known.

use sqlparser::ast as sql;
use straum_domain::{Identifier, Ref, RefKind, Table, Via};

use super::columns::{Resolved, resolve};
use super::{JoinedRef, Scope};
use crate::sql::{NamedTable, Reported, Walker, and_parts, line, written_digits};

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

/// Why one part of a join's ON gives no key equality.
enum NotEquality {
    /// The part has not the shape of one; the caller names the whole ON.
    Shape,
    /// The part's own error is reported already.
    Reported,
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
    // Hands back WHERE's conditions borrowed from the tree, which outlives
    // the walk.
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
            if let Ok(joined) = self.join(&scope, join) {
                self.add_ref(&mut scope, line(join), joined);
            }
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
            if let Ok(joined) = self.exists(&scope, part, subquery, kind) {
                self.add_ref(&mut scope, line(part), joined);
            }
        }
        Ok(FromClause {
            table,
            scope,
            conditions,
        })
    }

    /// Adds a join to the scope, unless the fact or a join before it
    /// already goes by its alias.
    fn add_ref(&mut self, scope: &mut Scope, at: u64, joined: JoinedRef) {
        let alias = &joined.reference.alias;
        if alias_taken(scope, alias) {
            self.error(at, format!("alias {alias} used twice"));
            return;
        }
        scope.refs.push(joined);
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
        let clauses_at = self.select_clauses(select);
        self.reject_if_present(clauses_at, select.having.is_some(), "HAVING");
        if !selects_one(&select.projection) {
            self.refuse(at, "EXISTS, write EXISTS (SELECT 1 FROM ...),", expression);
            return Err(Reported);
        }
        self.reject_if_present(
            at,
            !groups_by_nothing(&select.group_by),
            "GROUP BY inside EXISTS",
        );
        let [from] = select.from.as_slice() else {
            self.refuse(
                at,
                "EXISTS, write EXISTS (SELECT 1 FROM one table),",
                expression,
            );
            return Err(Reported);
        };
        self.reject_if_present(at, !from.joins.is_empty(), "a join inside EXISTS");
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
            let equality = match self.equality(scope, alias, part) {
                Ok(equality) => equality,
                Err(NotEquality::Reported) => return Err(Reported),
                Err(NotEquality::Shape) => {
                    self.error(
                        line(on),
                        format!(
                            "join ON {alias}.<key> = {}.<column>, the joined table's column \
                             on the left, one or more joined by AND; found {on}",
                            scope.alias
                        ),
                    );
                    return Err(Reported);
                }
            };
            key.push(equality.key);
            via.push(equality.via);
        }
        Ok(Equalities { key, via })
    }

    /// One key equality; `NotEquality::Shape` when the part is none,
    /// which the caller reports for the whole ON.
    fn equality(
        &mut self,
        scope: &Scope,
        alias: &Identifier,
        expression: &sql::Expr,
    ) -> Result<Equality, NotEquality> {
        let sql::Expr::BinaryOp {
            left,
            op: sql::BinaryOperator::Eq,
            right,
        } = expression
        else {
            return Err(NotEquality::Shape);
        };
        // A literal side, such as `true`, makes the part no key equality,
        // which the whole ON says best. Any other side says only its own
        // error.
        if is_value(left) || is_value(right) {
            return Err(NotEquality::Shape);
        }
        // Both sides are walked before either is judged, so each reports
        // its own errors.
        let joined_side = self.qualified(left);
        let near_side = self.qualified(right);
        let (Ok(joined_side), Ok(near_side)) = (joined_side, near_side) else {
            return Err(NotEquality::Reported);
        };
        if joined_side.alias != *alias {
            return Err(NotEquality::Shape);
        }
        let via = match resolve(scope, near_side) {
            Resolved::Column(via) => via,
            Resolved::UnknownAlias(_) => return Err(NotEquality::Shape),
            Resolved::Unreadable(unreadable) => {
                self.report_unreadable(line(right.as_ref()), scope, unreadable);
                return Err(NotEquality::Reported);
            }
        };
        Ok(Equality {
            key: joined_side.column,
            via,
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
        self.reject_if_present(at, !columns.is_empty(), "an alias column list");
        self.reject_if_present(at, at_time.is_some(), "AT");
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
        self.reject_if_present(at, *global, "GLOBAL");
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
    let [sql::SelectItem::UnnamedExpr(expression)] = projection else {
        return false;
    };
    written_digits(expression) == Some("1")
}

/// Whether a join side is a literal value, such as `1` or `true`.
fn is_value(expression: &sql::Expr) -> bool {
    matches!(expression, sql::Expr::Value(_))
}

fn groups_by_nothing(group_by: &sql::GroupByExpr) -> bool {
    match group_by {
        sql::GroupByExpr::Expressions(expressions, _) => expressions.is_empty(),
        sql::GroupByExpr::All(_) => false,
    }
}

fn alias_taken(scope: &Scope, alias: &Identifier) -> bool {
    *alias == scope.alias || ref_position(scope, alias).is_some()
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
