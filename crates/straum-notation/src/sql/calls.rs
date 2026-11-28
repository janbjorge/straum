//! The function calls both walks read. A call is checked for every option
//! Straum SQL has no use for before its arguments are read; its name is
//! found in any case, then refused unless lowercase. `buckets` is the one
//! call a view and an ask both write: a view's names its time and every
//! width kept, an ask's a wider width to read it at. Both read the call
//! and its widths here, so each refuses the same things in the same words.
//! `keep('<width>', '<duration>')` stands in for a width of a view's
//! `buckets`, keeping that width only for the duration; anywhere else it
//! is refused, naming the one place it goes.

use sqlparser::ast as sql;
use straum_domain::{Retention, UNITS, Width};

use super::{Reported, Walker, all_digits, line};

/// The function a view's time is written with, and an ask's wider width.
pub(super) const BUCKETS: &str = "buckets";

/// PostgreSQL's function for a bucket, refused for `buckets`.
pub(super) const DATE_BIN: &str = "date_bin";

/// The call that keeps a width of a view's `buckets` only for a time.
pub(super) const KEEP: &str = "keep";

/// How `keep` is written.
const KEEP_FORM: &str = "keep('<width>', '<duration>')";

/// What to write for a width that can't be read.
const WIDTH_FORM: &str = "use '<n> seconds|minutes|hours|days'";

/// A width a view's `buckets` keeps.
pub(super) enum KeptWidth {
    ForGood(Width),
    ForATime(Retention),
}

/// A call's arguments and how it treats them.
// Borrows from the tree, which outlives the walk.
pub(super) struct Call<'tree> {
    pub(super) arguments: &'tree [sql::FunctionArg],
    pub(super) distinct: bool,
    pub(super) within_group: &'tree [sql::OrderByExpr],
}

impl Walker {
    /// The arguments of a `buckets` call, after refusing FILTER, DISTINCT
    /// and anything else a plain call refuses.
    // Borrows the arguments from the tree, which outlives the walk.
    pub(super) fn buckets_arguments<'tree>(
        &mut self,
        function: &'tree sql::Function,
    ) -> Result<&'tree [sql::FunctionArg], Reported> {
        self.width_call_arguments(function, BUCKETS)
    }

    /// The arguments of a call to `name`, `buckets` or `keep`, after
    /// refusing FILTER, DISTINCT and anything else a plain call refuses.
    // Borrows the arguments from the tree, which outlives the walk.
    fn width_call_arguments<'tree>(
        &mut self,
        function: &'tree sql::Function,
        name: &str,
    ) -> Result<&'tree [sql::FunctionArg], Reported> {
        let at = line(function);
        self.reject_if_present(at, function.filter.is_some(), &format!("FILTER on {name}"));
        let Call {
            arguments,
            distinct,
            ..
        } = self.call(function)?;
        self.require_lowercase_name(function);
        self.reject_if_present(at, distinct, &format!("DISTINCT on {name}"));
        Ok(arguments)
    }

    /// A width of a view's `buckets`: `'<n> <unit>'`, kept for good, or
    /// `keep('<width>', '<duration>')`, kept only for the duration.
    pub(super) fn kept_width(&mut self, width: &sql::Expr) -> Result<KeptWidth, Reported> {
        let sql::Expr::Function(function) = width else {
            return Ok(KeptWidth::ForGood(self.bucket_width(width)?));
        };
        if !is_call(function, KEEP) {
            return Ok(KeptWidth::ForGood(self.bucket_width(width)?));
        }
        let arguments = self.width_call_arguments(function, KEEP)?;
        let [
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(kept)),
            sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(kept_for)),
        ] = arguments
        else {
            let count = arguments.len();
            let plural = if count == 1 { "" } else { "s" };
            let message = format!("{function} has {count} argument{plural}; write {KEEP_FORM}");
            self.error(line(function), message);
            return Err(Reported);
        };
        let kept = self.bucket_width(kept);
        let kept_for = self.bucket_width(kept_for);
        Ok(KeptWidth::ForATime(Retention {
            width: kept?,
            kept_for: kept_for?,
        }))
    }

    /// Refuses `expression` when it is a `keep` call, which goes only in
    /// place of a view's width; whether it was one.
    pub(super) fn refused_keep(&mut self, expression: &sql::Expr) -> bool {
        let sql::Expr::Function(function) = expression else {
            return false;
        };
        if !is_call(function, KEEP) {
            return false;
        }
        let message =
            format!("keep goes only in a view's buckets, in place of a width: {KEEP_FORM}");
        self.error(line(function), message);
        true
    }

    /// A width a `buckets` call writes, `'<n> <unit>'`; one that can't be
    /// read is reported, saying what to write.
    pub(super) fn bucket_width(&mut self, width: &sql::Expr) -> Result<Width, Reported> {
        match width_literal(width) {
            Ok(parsed) => Ok(parsed),
            Err(fix) => {
                self.error(line(width), format!("bucket width {width}: {fix}"));
                Err(Reported)
            }
        }
    }

    /// A function Straum SQL lists, found in any case, is named in
    /// lowercase as every name is: `COUNT` is refused, saying `count`.
    pub(super) fn require_lowercase_name(&mut self, function: &sql::Function) {
        let Some(written) = function_name(function) else {
            return;
        };
        let lower = written.to_ascii_lowercase();
        if written != lower {
            self.error(
                line(function),
                format!("{written}: write names in lowercase, {lower}"),
            );
        }
    }

    /// The arguments of a plain call `name(args)`, after rejecting
    /// anything else about the call.
    // Borrows the arguments from the tree, which outlives the walk.
    pub(super) fn call<'tree>(
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
    /// for the aggregates that take it, `percentile_disc` and `mode`.
    // Borrows the arguments from the tree, which outlives the walk.
    pub(super) fn ordered_call<'tree>(
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

/// Whether the call is to the function `name`, in any case, so a name
/// in capitals is refused as itself and not as some other call.
pub(super) fn is_call(function: &sql::Function, name: &str) -> bool {
    match function_name(function) {
        Some(written) => written.eq_ignore_ascii_case(name),
        None => false,
    }
}

/// A call's name, when it is one unquoted word.
pub(super) fn function_name(function: &sql::Function) -> Option<&str> {
    match function.name.0.as_slice() {
        [sql::ObjectNamePart::Identifier(identifier)] if identifier.quote_style.is_none() => {
            Some(&identifier.value)
        }
        _ => None,
    }
}

/// `'<n> <unit>'`, a width as a view's and an ask's `buckets` write it.
fn width_literal(width: &sql::Expr) -> Result<Width, String> {
    let sql::Expr::Value(sql::ValueWithSpan {
        value: sql::Value::SingleQuotedString(text),
        span: _,
    }) = width
    else {
        return Err(WIDTH_FORM.into());
    };
    parse_width(text)
}

/// A width `'<n> <unit>'`, written one way: `n` plain digits, the
/// largest unit that divides it, singular for one. `Err` says what to
/// write instead.
fn parse_width(text: &str) -> Result<Width, String> {
    let [count, unit] = text.split_whitespace().collect::<Vec<_>>()[..] else {
        return Err(WIDTH_FORM.into());
    };
    let name = unit.strip_suffix('s').unwrap_or(unit);
    let mut scale = None;
    for candidate in UNITS {
        if candidate.name == name {
            scale = Some(candidate.seconds);
        }
    }
    let (Some(scale), true) = (scale, !count.is_empty() && all_digits(count)) else {
        return Err(WIDTH_FORM.into());
    };
    let Ok(count) = count.parse::<u32>() else {
        return Err(WIDTH_FORM.into());
    };
    // No spelling of a zero width is kept, so none is offered.
    if count == 0 {
        return Err(WIDTH_FORM.into());
    }
    let Some(seconds) = count.checked_mul(scale) else {
        return Err(WIDTH_FORM.into());
    };
    let width = Width { seconds };
    let written = width.to_string();
    if text != written {
        return Err(format!("write '{written}'"));
    }
    Ok(width)
}
