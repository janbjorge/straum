//! The function calls a view's SELECT holds: the one `date_bin` bucket,
//! and the aggregates that become its measures, each with its own
//! FILTER. A call is checked for every option Straum SQL has no use for
//! before its arguments are read.

use sqlparser::ast as sql;
use straum_domain::{Aggregate, Condition, Fraction, Identifier, Measure, Origin, Time, Width};

use super::{Scope, parse_width};
use crate::sql::{Reported, Walker, line};

/// A call's arguments and how it treats them.
// Borrows from the tree, which outlives the walk.
struct Call<'tree> {
    args: &'tree [sql::FunctionArg],
    distinct: bool,
    within_group: &'tree [sql::OrderByExpr],
}

/// An aggregate and the FILTER it counts under.
struct Aggregated {
    aggregate: Aggregate,
    filter: Vec<Condition>,
}

/// An aggregate over one column, before the column is read.
#[derive(Clone, Copy)]
enum ColumnAggregate {
    CountNotNull,
    CountDistinct,
    Sum,
    Min,
    Max,
    Avg,
    BoolAnd,
    BoolOr,
}

impl ColumnAggregate {
    fn over(self, column: Identifier) -> Aggregate {
        match self {
            ColumnAggregate::CountNotNull => Aggregate::CountNotNull(column),
            ColumnAggregate::CountDistinct => Aggregate::CountDistinct(column),
            ColumnAggregate::Sum => Aggregate::Sum(column),
            ColumnAggregate::Min => Aggregate::Min(column),
            ColumnAggregate::Max => Aggregate::Max(column),
            ColumnAggregate::Avg => Aggregate::Avg(column),
            ColumnAggregate::BoolAnd => Aggregate::BoolAnd(column),
            ColumnAggregate::BoolOr => Aggregate::BoolOr(column),
        }
    }
}

/// What a plain aggregate counts: rows, as `count(*)` does, or a column.
enum Over {
    Rows,
    Column(ColumnAggregate),
}

/// A fraction's digits before and after its point.
struct Decimal<'text> {
    whole: &'text str,
    places: &'text str,
}

impl Walker {
    pub(super) fn measure(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        alias: &sql::Ident,
    ) -> Result<Measure, Reported> {
        let name = self.read_identifier(alias);
        let Aggregated { aggregate, filter } = self.aggregate(scope, function)?;
        Ok(Measure {
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
    ) -> Result<Time, Reported> {
        // Walked for its errors: the name is fixed.
        let _ = self.read_identifier(alias);
        if alias.value != "bucket" {
            self.error(
                alias.span.start.line,
                format!("the date_bin bucket is named AS bucket, not AS {alias}"),
            );
        }
        let at = line(function);
        if function.filter.is_some() {
            self.not_in_subset(at, "FILTER on date_bin");
        }
        let Call { args, distinct, .. } = self.call(function)?;
        if distinct {
            self.not_in_subset(at, "DISTINCT on date_bin");
        }
        let [
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(width)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(column)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(origin)),
        ] = args
        else {
            self.refuse(at, "date_bin arguments", function);
            return Err(Reported);
        };
        let parsed = date_bin_width(width);
        if parsed.is_none() {
            self.error(
                line(width),
                format!("date_bin width {width}: use '<n> seconds|minutes|hours|days'"),
            );
        }
        let epoch = date_bin_origin(origin);
        if epoch.is_none() {
            self.error(
                line(origin),
                format!("date_bin origin {origin}: use timestamptz 'epoch' or timestamp 'epoch'"),
            );
        }
        let column = self.uncast_fact_column(scope, column, "the date_bin column")?;
        let Some(width) = parsed else {
            return Err(Reported);
        };
        let Some(origin) = epoch else {
            return Err(Reported);
        };
        Ok(Time {
            column,
            width,
            wider: Vec::new(),
            origin,
        })
    }

    fn aggregate(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
    ) -> Result<Aggregated, Reported> {
        let at = line(function);
        let name = function_name(function).map(str::to_ascii_lowercase);
        let call = self.ordered_call(function)?;
        let filter_condition = function.filter.as_deref();
        match name.as_deref() {
            Some("percentile_disc") => {
                let aggregate = self.percentile_disc(scope, function, &call);
                let filter = self.read_filter(scope, filter_condition.as_slice())?;
                return Ok(Aggregated {
                    aggregate: aggregate?,
                    filter,
                });
            }
            Some("percentile_cont") => {
                self.refuse(at, "percentile_cont, use percentile_disc,", function);
                return Err(Reported);
            }
            _ => {
                if !call.within_group.is_empty() {
                    self.not_in_subset(at, "WITHIN GROUP");
                }
            }
        }
        let aggregate = match self.over(function, name.as_deref(), &call)? {
            Over::Rows => Aggregate::Count,
            Over::Column(aggregate) => {
                let [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(argument))] = call.args
                else {
                    self.refuse(at, "arguments", function);
                    return Err(Reported);
                };
                let column = self.uncast_fact_column(scope, argument, "an aggregated column")?;
                aggregate.over(column)
            }
        };
        let filter = self.read_filter(scope, filter_condition.as_slice())?;
        Ok(Aggregated { aggregate, filter })
    }

    /// Which aggregate a plain call names, by its name and DISTINCT.
    fn over(
        &mut self,
        function: &sql::Function,
        name: Option<&str>,
        call: &Call,
    ) -> Result<Over, Reported> {
        let at = line(function);
        let Some(name) = name else {
            self.refuse(at, "aggregate", function);
            return Err(Reported);
        };
        if name == "count" && !call.distinct {
            match call.args {
                [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Wildcard)] => {
                    return Ok(Over::Rows);
                }
                [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(sql::Expr::Value(_)))] => {
                    self.refuse(at, "count of a constant, use count(*),", function);
                    return Err(Reported);
                }
                _ => return Ok(Over::Column(ColumnAggregate::CountNotNull)),
            }
        }
        if name == "count" {
            return Ok(Over::Column(ColumnAggregate::CountDistinct));
        }
        if name == "every" {
            self.refuse(at, "every, write bool_and,", function);
            return Err(Reported);
        }
        let aggregate = match name {
            "sum" => ColumnAggregate::Sum,
            "min" => ColumnAggregate::Min,
            "max" => ColumnAggregate::Max,
            "avg" => ColumnAggregate::Avg,
            "bool_and" => ColumnAggregate::BoolAnd,
            "bool_or" => ColumnAggregate::BoolOr,
            _ => {
                self.refuse(at, "aggregate", function);
                return Err(Reported);
            }
        };
        if call.distinct {
            self.refuse(at, "DISTINCT outside count", function);
            return Err(Reported);
        }
        Ok(Over::Column(aggregate))
    }

    /// `percentile_disc(fraction) WITHIN GROUP (ORDER BY alias.column)`,
    /// ascending as written: PostgreSQL's only spelling of one percentile.
    fn percentile_disc(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        call: &Call,
    ) -> Result<Aggregate, Reported> {
        let at = line(function);
        if call.distinct {
            self.not_in_subset(at, "DISTINCT outside count");
        }
        let fraction =
            if let [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(argument))] = call.args {
                self.fraction(argument)
            } else {
                self.refuse(at, "arguments, write one fraction,", function);
                Err(Reported)
            };
        let [order] = call.within_group else {
            let what = match call.within_group {
                [] => "percentile_disc without WITHIN GROUP (ORDER BY alias.column)",
                _ => "WITHIN GROUP of more than one column",
            };
            self.refuse(at, what, function);
            return Err(Reported);
        };
        let sql::OrderByExpr {
            expr: column,
            options: sql::OrderByOptions { sort, nulls_first },
            with_fill,
        } = order;
        // Ascending is the only order, so it is never written.
        if sort.is_some() {
            self.not_in_subset(at, "ASC, DESC or USING in WITHIN GROUP");
        }
        if nulls_first.is_some() {
            self.not_in_subset(at, "NULLS FIRST or LAST");
        }
        if with_fill.is_some() {
            self.not_in_subset(at, "WITH FILL");
        }
        let column = self.uncast_fact_column(scope, column, "an aggregated column");
        Ok(Aggregate::PercentileDisc {
            fraction: fraction?,
            column: column?,
        })
    }

    /// A fraction from 0 to 1 as digits, with places after a point or
    /// none: exactly as written, never through a float.
    fn fraction(&mut self, expression: &sql::Expr) -> Result<Fraction, Reported> {
        let at = line(expression);
        let text = match expression {
            sql::Expr::Value(sql::ValueWithSpan {
                value: sql::Value::Number(text, false),
                span: _,
            }) => text.as_str(),
            _ => "",
        };
        let Decimal { whole, places } = split_decimal(text);
        if whole.is_empty() || !all_digits(whole) || !all_digits(places) || text.ends_with('.') {
            self.refuse(at, "fraction, write a number from 0 to 1,", expression);
            return Err(Reported);
        }
        let Some(fraction) = fraction_of(whole, places) else {
            self.error(
                at,
                format!("fraction {text}: write a number from 0 to 1 in at most 19 places"),
            );
            return Err(Reported);
        };
        Ok(fraction)
    }

    /// The arguments of a plain call `name(args)`, after rejecting
    /// anything else about the call.
    // Borrows the arguments from the tree, which outlives the walk.
    fn call<'tree>(&mut self, function: &'tree sql::Function) -> Result<Call<'tree>, Reported> {
        let call = self.ordered_call(function)?;
        if !call.within_group.is_empty() {
            self.not_in_subset(line(function), "WITHIN GROUP");
        }
        Ok(call)
    }

    /// As [`Walker::call`], with what `WITHIN GROUP (ORDER BY ...)` lists,
    /// for the one aggregate that takes it.
    fn ordered_call<'tree>(
        &mut self,
        function: &'tree sql::Function,
    ) -> Result<Call<'tree>, Reported> {
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
        if *uses_odbc_syntax {
            self.not_in_subset(at, "ODBC call syntax");
        }
        if !matches!(parameters, sql::FunctionArguments::None) {
            self.not_in_subset(at, "function parameters");
        }
        if null_treatment.is_some() {
            self.not_in_subset(at, "IGNORE or RESPECT NULLS");
        }
        if over.is_some() {
            self.not_in_subset(at, "a window function (OVER)");
        }
        let sql::FunctionArguments::List(sql::FunctionArgumentList {
            duplicate_treatment,
            args,
            clauses,
        }) = args
        else {
            self.refuse(at, "function call", function);
            return Err(Reported);
        };
        if !clauses.is_empty() {
            self.not_in_subset(at, "ORDER BY or LIMIT inside a call");
        }
        let distinct = match duplicate_treatment {
            None => false,
            Some(sql::DuplicateTreatment::Distinct) => true,
            Some(sql::DuplicateTreatment::All) => {
                self.not_in_subset(at, "ALL inside a call");
                false
            }
        };
        Ok(Call {
            args,
            distinct,
            within_group,
        })
    }
}

/// `'<n> <unit>'` as `date_bin`'s first argument.
fn date_bin_width(width: &sql::Expr) -> Option<Width> {
    let sql::Expr::Value(sql::ValueWithSpan {
        value: sql::Value::SingleQuotedString(text),
        span: _,
    }) = width
    else {
        return None;
    };
    parse_width(text)
}

/// `timestamptz 'epoch'` or `timestamp 'epoch'`, `date_bin`'s origin.
fn date_bin_origin(origin: &sql::Expr) -> Option<Origin> {
    let sql::Expr::TypedString(sql::TypedString {
        data_type: sql::DataType::Timestamp(None, zone),
        value:
            sql::ValueWithSpan {
                value: sql::Value::SingleQuotedString(text),
                span: _,
            },
        uses_odbc_syntax: false,
    }) = origin
    else {
        return None;
    };
    if text != "epoch" {
        return None;
    }
    match zone {
        sql::TimezoneInfo::Tz => Some(Origin::Timestamptz),
        sql::TimezoneInfo::None => Some(Origin::Timestamp),
        sql::TimezoneInfo::WithTimeZone | sql::TimezoneInfo::WithoutTimeZone => None,
    }
}

fn split_decimal(text: &str) -> Decimal<'_> {
    match text.split_once('.') {
        Some((whole, places)) => Decimal { whole, places },
        None => Decimal {
            whole: text,
            places: "",
        },
    }
}

fn all_digits(text: &str) -> bool {
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return false;
        }
    }
    true
}

fn fraction_of(whole: &str, places: &str) -> Option<Fraction> {
    let units = format!("{whole}{places}").parse().ok()?;
    let places = u32::try_from(places.len()).ok()?;
    Fraction::new(units, places)
}

/// Whether the call is `date_bin`, the bucket.
pub(super) fn is_date_bin(function: &sql::Function) -> bool {
    match function_name(function) {
        Some(name) => name.eq_ignore_ascii_case("date_bin"),
        None => false,
    }
}

/// A call's name, when it is one unquoted word.
fn function_name(function: &sql::Function) -> Option<&str> {
    match function.name.0.as_slice() {
        [sql::ObjectNamePart::Identifier(identifier)] if identifier.quote_style.is_none() => {
            Some(&identifier.value)
        }
        _ => None,
    }
}
