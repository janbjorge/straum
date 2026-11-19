//! The function calls a view's SELECT holds: the one `date_bin` bucket,
//! and the aggregates that become its measures, each with its own
//! FILTER. A call is checked for every option Straum SQL has no use for
//! before its arguments are read.

use sqlparser::ast as sql;
use straum_domain::{Aggregate, Condition, Fraction, Identifier, Measure, Origin, Time, Width};

use super::{Scope, WIDTH_FORM, parse_width};
use crate::sql::{Clause, Reported, Walker, line};

/// A call's arguments and how it treats them.
// Borrows from the tree, which outlives the walk.
pub(in crate::sql) struct Call<'tree> {
    pub(in crate::sql) arguments: &'tree [sql::FunctionArg],
    pub(in crate::sql) distinct: bool,
    within_group: &'tree [sql::OrderByExpr],
}

/// An aggregate and the FILTER it counts under.
struct Aggregated {
    aggregate: Aggregate,
    filter: Vec<Condition>,
}

/// What a plain aggregate counts: rows, as `count(*)` does, or a column.
enum Over {
    Rows,
    /// The aggregate's variant, as a function from the column it reads,
    /// which comes later.
    Column(fn(Identifier) -> Aggregate),
}

/// A fraction's digits before and after its point.
// Two slices of the fraction as written, so nothing is copied.
struct Decimal<'text> {
    whole: &'text str,
    places: &'text str,
}

impl Walker {
    /// A measure item: an aggregate `AS` its name.
    pub(super) fn measure(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        alias: &sql::Ident,
    ) -> Result<Measure, Reported> {
        let name = self.read_output_name(alias);
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
        self.reject_if_present(at, function.filter.is_some(), "FILTER on date_bin");
        let Call {
            arguments,
            distinct,
            ..
        } = self.call(function)?;
        self.reject_if_present(at, distinct, "DISTINCT on date_bin");
        let [
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(width)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(column)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(origin)),
        ] = arguments
        else {
            self.refuse(at, "date_bin arguments", function);
            return Err(Reported);
        };
        let parsed = date_bin_width(width);
        if let Err(fix) = &parsed {
            self.error(line(width), format!("date_bin width {width}: {fix}"));
        }
        let parsed = parsed.ok();
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
                let filter =
                    self.read_filter(scope, filter_condition.as_slice(), Clause::Filter)?;
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
                self.reject_if_present(at, !call.within_group.is_empty(), "WITHIN GROUP");
            }
        }
        let aggregate = match self.over(function, name.as_deref(), &call)? {
            Over::Rows => Aggregate::Count,
            Over::Column(aggregate) => {
                let [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(argument))] =
                    call.arguments
                else {
                    self.refuse(at, "arguments", function);
                    return Err(Reported);
                };
                let column = self.uncast_fact_column(scope, argument, "an aggregated column")?;
                aggregate(column)
            }
        };
        let filter = self.read_filter(scope, filter_condition.as_slice(), Clause::Filter)?;
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
            match call.arguments {
                [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Wildcard)] => {
                    return Ok(Over::Rows);
                }
                [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(sql::Expr::Value(_)))] => {
                    self.refuse(at, "count of a constant, use count(*),", function);
                    return Err(Reported);
                }
                _ => return Ok(Over::Column(Aggregate::CountNotNull)),
            }
        }
        if name == "count" {
            return Ok(Over::Column(Aggregate::CountDistinct));
        }
        if name == "every" {
            self.refuse(at, "every, write bool_and,", function);
            return Err(Reported);
        }
        if name == "variance" {
            self.refuse(at, "variance, write var_samp,", function);
            return Err(Reported);
        }
        if name == "stddev" {
            self.refuse(at, "stddev, write stddev_samp,", function);
            return Err(Reported);
        }
        let aggregate = match name {
            "sum" => Aggregate::Sum,
            "min" => Aggregate::Min,
            "max" => Aggregate::Max,
            "avg" => Aggregate::Avg,
            "bool_and" => Aggregate::BoolAnd,
            "bool_or" => Aggregate::BoolOr,
            "var_samp" => Aggregate::VarSamp,
            "var_pop" => Aggregate::VarPop,
            "stddev_samp" => Aggregate::StddevSamp,
            "stddev_pop" => Aggregate::StddevPop,
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
        self.reject_if_present(at, call.distinct, "DISTINCT outside count");
        let fraction = if let [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(argument))] =
            call.arguments
        {
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
        self.reject_if_present(at, sort.is_some(), "ASC, DESC or USING in WITHIN GROUP");
        self.reject_if_present(at, nulls_first.is_some(), "NULLS FIRST or LAST");
        self.reject_if_present(at, with_fill.is_some(), "WITH FILL");
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
    pub(in crate::sql) fn call<'tree>(
        &mut self,
        function: &'tree sql::Function,
    ) -> Result<Call<'tree>, Reported> {
        let call = self.ordered_call(function)?;
        self.reject_if_present(
            line(function),
            !call.within_group.is_empty(),
            "WITHIN GROUP",
        );
        Ok(call)
    }

    /// As [`Walker::call`], with what `WITHIN GROUP (ORDER BY ...)` lists,
    /// for the one aggregate that takes it.
    // Borrows the arguments from the tree, which outlives the walk.
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
        self.reject_if_present(at, *uses_odbc_syntax, "ODBC call syntax");
        self.reject_if_present(
            at,
            !matches!(parameters, sql::FunctionArguments::None),
            "function parameters",
        );
        self.reject_if_present(at, null_treatment.is_some(), "IGNORE or RESPECT NULLS");
        self.reject_if_present(at, over.is_some(), "a window function (OVER)");
        let sql::FunctionArguments::List(sql::FunctionArgumentList {
            duplicate_treatment,
            args,
            clauses,
        }) = args
        else {
            self.refuse(at, "function call", function);
            return Err(Reported);
        };
        self.reject_if_present(at, !clauses.is_empty(), "ORDER BY or LIMIT inside a call");
        let distinct = match duplicate_treatment {
            None => false,
            Some(sql::DuplicateTreatment::Distinct) => true,
            Some(sql::DuplicateTreatment::All) => {
                self.not_in_subset(at, "ALL inside a call");
                false
            }
        };
        Ok(Call {
            arguments: args,
            distinct,
            within_group,
        })
    }
}

/// `'<n> <unit>'` as `date_bin`'s first argument.
pub(in crate::sql) fn date_bin_width(width: &sql::Expr) -> Result<Width, String> {
    let sql::Expr::Value(sql::ValueWithSpan {
        value: sql::Value::SingleQuotedString(text),
        span: _,
    }) = width
    else {
        return Err(WIDTH_FORM.into());
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
pub(in crate::sql) fn is_date_bin(function: &sql::Function) -> bool {
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
