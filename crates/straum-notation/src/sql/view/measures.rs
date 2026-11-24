//! The function calls a view's SELECT holds: the one `buckets` item,
//! the view's time, and the aggregates that become its measures, each with its own
//! FILTER. `calls`, beside the walker, checks a call for every option
//! Straum SQL has no use for before its arguments are read here.

use sqlparser::ast as sql;
use straum_domain::{Aggregate, Condition, Fraction, Identifier, Measure, Time};

use super::Scope;
use crate::sql::calls::{Call, KeptWidth, function_name};
use crate::sql::{Clause, Reported, Walker, all_digits, line, written_digits};

/// What DISTINCT on any other aggregate is refused as.
const DISTINCT_OUTSIDE: &str = "DISTINCT outside count, sum and avg";

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

    /// `buckets(f.column, '1 hour', '1 day')`: the column the view's
    /// time reads, then every width kept, narrowest first, any of them
    /// written `keep('<width>', '<duration>')` to keep it only for a time.
    /// Rows go in buckets of the first; `Spec::new` checks each is a whole
    /// multiple of the one before, and the times kept.
    pub(super) fn bucket(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        alias: &sql::Ident,
    ) -> Result<Time, Reported> {
        let name = self.read_output_name(alias);
        let at = line(function);
        let arguments = self.buckets_arguments(function)?;
        let [
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(column)),
            widths @ ..,
        ] = arguments
        else {
            self.refuse(at, "buckets arguments", function);
            return Err(Reported);
        };
        if self.refused_keep(column) {
            return Err(Reported);
        }
        let column = self.uncast_fact_column(scope, column, "the buckets column");
        let mut kept = Vec::with_capacity(widths.len());
        let mut retained = Vec::new();
        for width in widths {
            let sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(width)) = width else {
                self.refuse(at, "buckets arguments", function);
                return Err(Reported);
            };
            match self.kept_width(width) {
                Ok(KeptWidth::ForGood(width)) => kept.push(width),
                Ok(KeptWidth::ForATime(retention)) => {
                    kept.push(retention.width);
                    retained.push(retention);
                }
                Err(Reported) => {}
            }
        }
        if widths.is_empty() {
            self.error(
                at,
                "buckets needs a width after the column: buckets(<column>, '<n> <unit>', ...)"
                    .into(),
            );
        }
        let column = column?;
        let name = name?;
        let [width, wider @ ..] = kept.as_slice() else {
            return Err(Reported);
        };
        if kept.len() < widths.len() {
            return Err(Reported);
        }
        Ok(Time {
            name,
            column,
            width: *width,
            wider: wider.to_vec(),
            retained,
        })
    }

    /// An aggregate Straum SQL lists, its name in lowercase.
    fn aggregate(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
    ) -> Result<Aggregated, Reported> {
        let aggregated = self.listed_aggregate(scope, function)?;
        self.require_lowercase_name(function);
        Ok(aggregated)
    }

    fn listed_aggregate(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
    ) -> Result<Aggregated, Reported> {
        let at = line(function);
        let name = function_name(function).map(str::to_ascii_lowercase);
        let call = self.ordered_call(function)?;
        // Both sides are walked before either error ends the measure, so
        // a bad aggregate and a bad FILTER are reported in one pass.
        let aggregate = match name.as_deref() {
            Some("percentile_disc") => self.percentile_disc(scope, function, &call),
            Some("mode") => self.mode(scope, function, &call),
            Some("percentile_cont") => {
                self.refuse(at, "percentile_cont, use percentile_disc,", function);
                Err(Reported)
            }
            _ => self.plain_aggregate(scope, function, name.as_deref(), &call),
        };
        let filter_condition = function.filter.as_deref();
        let filter = self.read_filter(scope, filter_condition.as_slice(), Clause::Filter)?;
        Ok(Aggregated {
            aggregate: aggregate?,
            filter,
        })
    }

    /// An aggregate with no WITHIN GROUP: `count(*)`, or one over a column.
    fn plain_aggregate(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        name: Option<&str>,
        call: &Call,
    ) -> Result<Aggregate, Reported> {
        let at = line(function);
        self.reject_if_present(at, !call.within_group.is_empty(), "WITHIN GROUP");
        match self.over(function, name, call)? {
            Over::Rows => Ok(Aggregate::Count),
            Over::Column(aggregate) => {
                let [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(argument))] =
                    call.arguments
                else {
                    self.refuse(at, "arguments", function);
                    return Err(Reported);
                };
                let column = self.uncast_fact_column(scope, argument, "an aggregated column")?;
                Ok(aggregate(column))
            }
        }
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
        if !call.distinct {
            return Ok(Over::Column(aggregate));
        }
        match name {
            "sum" => Ok(Over::Column(Aggregate::SumDistinct)),
            "avg" => Ok(Over::Column(Aggregate::AvgDistinct)),
            _ => {
                self.refuse(at, DISTINCT_OUTSIDE, function);
                Err(Reported)
            }
        }
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
        self.reject_if_present(at, call.distinct, DISTINCT_OUTSIDE);
        let fraction = if let [sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(argument))] =
            call.arguments
        {
            self.fraction(argument)
        } else {
            self.refuse(at, "arguments, write one fraction,", function);
            Err(Reported)
        };
        let column = self.ordered_column(scope, function, call, "percentile_disc");
        Ok(Aggregate::PercentileDisc {
            fraction: fraction?,
            column: column?,
        })
    }

    /// `mode() WITHIN GROUP (ORDER BY alias.column)`: no argument, the
    /// column in the order, as PostgreSQL spells it.
    fn mode(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        call: &Call,
    ) -> Result<Aggregate, Reported> {
        let at = line(function);
        self.reject_if_present(at, call.distinct, DISTINCT_OUTSIDE);
        if !call.arguments.is_empty() {
            self.refuse(
                at,
                "arguments, write mode() WITHIN GROUP (ORDER BY alias.column),",
                function,
            );
        }
        let column = self.ordered_column(scope, function, call, "mode()")?;
        if !call.arguments.is_empty() {
            return Err(Reported);
        }
        Ok(Aggregate::Mode(column))
    }

    /// The one column `WITHIN GROUP (ORDER BY alias.column)` names, for
    /// `aggregate`, ascending as written.
    fn ordered_column(
        &mut self,
        scope: &Scope,
        function: &sql::Function,
        call: &Call,
        aggregate: &str,
    ) -> Result<Identifier, Reported> {
        let at = line(function);
        let [order] = call.within_group else {
            let what = match call.within_group {
                [] => format!("{aggregate} without WITHIN GROUP (ORDER BY alias.column)"),
                _ => "WITHIN GROUP of more than one column".to_owned(),
            };
            self.refuse(at, &what, function);
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
        self.uncast_fact_column(scope, column, "an aggregated column")
    }

    /// A fraction from 0 to 1 as digits, with places after a point or
    /// none: exactly as written, never through a float. One spelling
    /// each, the shortest, so `0.50` and `1.0` say what to write.
    fn fraction(&mut self, expression: &sql::Expr) -> Result<Fraction, Reported> {
        let at = line(expression);
        let text = written_digits(expression).unwrap_or_default();
        let Decimal { whole, places } = split_decimal(text);
        if whole.is_empty() || !all_digits(whole) || !all_digits(places) || text.ends_with('.') {
            self.refuse(at, "fraction, write a number from 0 to 1,", expression);
            return Err(Reported);
        }
        let shortest = shortest_decimal(whole, places);
        let Some(fraction) = fraction_of(shortest.whole, shortest.places) else {
            self.error(
                at,
                format!("fraction {text}: write a number from 0 to 1 in at most 19 places"),
            );
            return Err(Reported);
        };
        let canonical = written_decimal(&shortest);
        if canonical != text {
            self.error(at, format!("fraction {text}: write {canonical}"));
            return Err(Reported);
        }
        Ok(fraction)
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

/// The decimal without leading zeros before its point or trailing ones
/// after it, keeping one zero where none would be left: `0.5`, `1`, `0`.
fn shortest_decimal<'text>(whole: &'text str, places: &'text str) -> Decimal<'text> {
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    Decimal {
        whole,
        places: places.trim_end_matches('0'),
    }
}

fn written_decimal(decimal: &Decimal<'_>) -> String {
    if decimal.places.is_empty() {
        return decimal.whole.to_owned();
    }
    format!("{}.{}", decimal.whole, decimal.places)
}

fn fraction_of(whole: &str, places: &str) -> Option<Fraction> {
    let units = format!("{whole}{places}").parse().ok()?;
    let places = u32::try_from(places.len()).ok()?;
    Fraction::new(units, places)
}
