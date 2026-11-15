use sqlparser::ast as sql;
use straum_domain::{Aggregate, Condition, Fraction, Identifier, Measure, Origin, Time};

use super::{Call, Scope, parse_width};
use crate::sql::{Walker, line};

impl Walker {
    pub(super) fn measure(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        alias: &sql::Ident,
    ) -> Option<Measure> {
        let name = self.read_identifier(alias);
        let (aggregate, filter) = self.aggregate(scope, function)?;
        Some(Measure {
            name: name?,
            aggregate,
            filter,
        })
    }

    /// `date_bin('15 minutes', f.column, timestamptz 'epoch')`, or
    /// `timestamp 'epoch'` for a column without a zone. The origin is fixed
    /// so buckets start on midnight, as Straum's do; which type it has, the
    /// catalog checks against the column.
    pub(super) fn bucket(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        alias: &sql::Ident,
    ) -> Option<Time> {
        // Walked for its errors: the name is fixed.
        self.read_identifier(alias);
        if alias.value != "bucket" {
            self.error(
                alias.span.start.line,
                format!("the date_bin bucket is named AS bucket, not AS {alias}"),
            );
        }
        let at = line(function);
        self.absent(at, function.filter.is_some(), "FILTER on date_bin");
        let Call { args, distinct, .. } = self.call(function)?;
        self.absent(at, distinct, "DISTINCT on date_bin");
        let [
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(width)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(column_expr)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(origin)),
        ] = args
        else {
            self.refuse(at, "date_bin arguments", function);
            return None;
        };
        let parsed = match width {
            sql::Expr::Value(sql::ValueWithSpan {
                value: sql::Value::SingleQuotedString(text),
                span: _,
            }) => parse_width(text),
            _ => None,
        };
        if parsed.is_none() {
            self.error(
                line(width),
                format!("date_bin width {width}: use '<n> seconds|minutes|hours|days'"),
            );
        }
        let epoch = match origin {
            sql::Expr::TypedString(sql::TypedString {
                data_type: sql::DataType::Timestamp(None, zone),
                value:
                    sql::ValueWithSpan {
                        value: sql::Value::SingleQuotedString(text),
                        span: _,
                    },
                uses_odbc_syntax: false,
            }) if text == "epoch" => match zone {
                sql::TimezoneInfo::Tz => Some(Origin::Timestamptz),
                sql::TimezoneInfo::None => Some(Origin::Timestamp),
                sql::TimezoneInfo::WithTimeZone | sql::TimezoneInfo::WithoutTimeZone => None,
            },
            _ => None,
        };
        if epoch.is_none() {
            self.error(
                line(origin),
                format!("date_bin origin {origin}: use timestamptz 'epoch' or timestamp 'epoch'"),
            );
        }
        let column = self.uncast_fact_column(scope, column_expr, "the date_bin column")?;
        Some(Time {
            column,
            width: parsed?,
            wider: Vec::new(),
            origin: epoch?,
        })
    }

    pub(super) fn aggregate(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
    ) -> Option<(Aggregate, Vec<Condition>)> {
        let at = line(function);
        let name = function_name(function).map(str::to_ascii_lowercase);
        let call = self.ordered_call(function)?;
        let Call {
            args,
            distinct,
            within_group,
        } = call;
        match name.as_deref() {
            Some("percentile_disc") => {
                let aggregate = self.percentile_disc(scope, function, &call);
                let filter = self.read_filter(scope, function.filter.as_deref())?;
                return Some((aggregate?, filter));
            }
            Some("percentile_cont") => {
                self.refuse(at, "percentile_cont, use percentile_disc,", function);
                return None;
            }
            _ => self.absent(at, !within_group.is_empty(), "WITHIN GROUP"),
        }
        let star = matches!(
            args,
            [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Wildcard)]
        );
        let single = match args {
            [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(expr))] => Some(expr),
            _ => None,
        };
        let constant = matches!(single, Some(sql::Expr::Value(_)));
        // `None` is count(*), the one aggregate over no column.
        let of_column: Option<fn(Identifier) -> Aggregate> = match (name.as_deref(), distinct) {
            (Some("count"), false) if star => None,
            (Some("count"), false) if constant => {
                self.refuse(at, "count of a constant, use count(*),", function);
                return None;
            }
            (Some("count"), false) => Some(Aggregate::CountNotNull),
            (Some("count"), true) => Some(Aggregate::CountDistinct),
            (Some("sum"), false) => Some(Aggregate::Sum),
            (Some("min"), false) => Some(Aggregate::Min),
            (Some("max"), false) => Some(Aggregate::Max),
            (Some("avg"), false) => Some(Aggregate::Avg),
            (Some("bool_and"), false) => Some(Aggregate::BoolAnd),
            (Some("bool_or"), false) => Some(Aggregate::BoolOr),
            (Some("every"), _) => {
                self.refuse(at, "every, write bool_and,", function);
                return None;
            }
            (Some("sum" | "min" | "max" | "avg" | "bool_and" | "bool_or"), true) => {
                self.refuse(at, "DISTINCT outside count", function);
                return None;
            }
            _ => {
                self.refuse(at, "aggregate", function);
                return None;
            }
        };
        let aggregate = match of_column {
            None => Aggregate::Count,
            Some(of_column) => {
                let Some(arg) = single else {
                    self.refuse(at, "arguments", function);
                    return None;
                };
                of_column(self.uncast_fact_column(scope, arg, "an aggregated column")?)
            }
        };
        let filter = self.read_filter(scope, function.filter.as_deref())?;
        Some((aggregate, filter))
    }

    /// `percentile_disc(fraction) WITHIN GROUP (ORDER BY alias.column)`,
    /// ascending as written: PostgreSQL's only spelling of one percentile.
    pub(super) fn percentile_disc(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        call: &Call,
    ) -> Option<Aggregate> {
        let &Call {
            args,
            distinct,
            within_group,
        } = call;
        let at = line(function);
        self.absent(at, distinct, "DISTINCT outside count");
        let fraction = if let [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(expr))] = args {
            self.fraction(expr)
        } else {
            self.refuse(at, "arguments, write one fraction,", function);
            None
        };
        let [order] = within_group else {
            let what = match within_group {
                [] => "percentile_disc without WITHIN GROUP (ORDER BY alias.column)",
                _ => "WITHIN GROUP of more than one column",
            };
            self.refuse(at, what, function);
            return None;
        };
        let sql::OrderByExpr {
            expr,
            options: sql::OrderByOptions { sort, nulls_first },
            with_fill,
        } = order;
        // Ascending is the only order, so it is never written.
        self.absent(at, sort.is_some(), "ASC, DESC or USING in WITHIN GROUP");
        self.absent(at, nulls_first.is_some(), "NULLS FIRST or LAST");
        self.absent(at, with_fill.is_some(), "WITH FILL");
        let column = self.uncast_fact_column(scope, expr, "an aggregated column");
        Some(Aggregate::PercentileDisc {
            fraction: fraction?,
            column: column?,
        })
    }

    /// A fraction from 0 to 1 as digits, with places after a point or
    /// none: exactly as written, never through a float.
    pub(super) fn fraction(&mut self, expr: &sql::Expr) -> Option<Fraction> {
        let at = line(expr);
        let text = match expr {
            sql::Expr::Value(sql::ValueWithSpan {
                value: sql::Value::Number(text, false),
                span: _,
            }) => text.as_str(),
            _ => "",
        };
        let (whole, places) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty() || !all_digits(whole) || !all_digits(places) || text.ends_with('.') {
            self.refuse(at, "fraction, write a number from 0 to 1,", expr);
            return None;
        }
        let fraction = fraction_of(whole, places);
        if fraction.is_none() {
            self.error(
                at,
                format!("fraction {text}: write a number from 0 to 1 in at most 19 places"),
            );
        }
        fraction
    }

    /// The arguments of a plain call `name(args)`, or `None` after rejecting
    /// anything else about the call.
    pub(super) fn call<'a>(&mut self, function: &'a sql::Function) -> Option<Call<'a>> {
        let call = self.ordered_call(function)?;
        self.absent(
            line(function),
            !call.within_group.is_empty(),
            "WITHIN GROUP",
        );
        Some(call)
    }

    /// As [`Walker::call`], with what `WITHIN GROUP (ORDER BY ...)` lists,
    /// for the one aggregate that takes it.
    pub(super) fn ordered_call<'a>(&mut self, function: &'a sql::Function) -> Option<Call<'a>> {
        let sql::Function {
            name: _,
            uses_odbc_syntax,
            parameters,
            args,
            within_group,
            filter: _,
            null_treatment,
            over,
        } = function;
        let at = line(function);
        self.absent(at, *uses_odbc_syntax, "ODBC call syntax");
        self.absent(
            at,
            !matches!(parameters, sql::FunctionArguments::None),
            "function parameters",
        );
        self.absent(at, null_treatment.is_some(), "IGNORE or RESPECT NULLS");
        self.absent(at, over.is_some(), "a window function (OVER)");
        let sql::FunctionArguments::List(sql::FunctionArgumentList {
            duplicate_treatment,
            args,
            clauses,
        }) = args
        else {
            self.refuse(at, "function call", function);
            return None;
        };
        self.absent(at, !clauses.is_empty(), "ORDER BY or LIMIT inside a call");
        let distinct = match duplicate_treatment {
            None => false,
            Some(sql::DuplicateTreatment::Distinct) => true,
            Some(sql::DuplicateTreatment::All) => {
                self.not_in_subset(at, "ALL inside a call");
                false
            }
        };
        Some(Call {
            args,
            distinct,
            within_group,
        })
    }
}

pub(super) fn all_digits(text: &str) -> bool {
    text.bytes().all(|byte| byte.is_ascii_digit())
}

pub(super) fn fraction_of(whole: &str, places: &str) -> Option<Fraction> {
    let units = format!("{whole}{places}").parse().ok()?;
    let places = u32::try_from(places.len()).ok()?;
    Fraction::new(units, places)
}

pub(super) fn is_date_bin(function: &sql::Function) -> bool {
    function_name(function).is_some_and(|name| name.eq_ignore_ascii_case("date_bin"))
}

pub(super) fn function_name(function: &sql::Function) -> Option<&str> {
    match function.name.0.as_slice() {
        [sql::ObjectNamePart::Identifier(id)] if id.quote_style.is_none() => Some(&id.value),
        _ => None,
    }
}
