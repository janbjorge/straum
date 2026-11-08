//! Text notation for a weir spec.
//!
//! Line-based. A block starts at column 0 with `dim` or `fact`; its body
//! lines are indented. `#` starts a comment.
//!
//! ```text
//! dim zones = public.taxi_zones key location_id
//!   attrs borough, zone
//!
//! fact trips = public.yellow_trips key id
//!   time pickup_at / 15m
//!   ref pickup -> zones via pu_location_id
//!   by payment_type
//!   append
//!   trips count
//!   fare sum fare_amount
//!   card count where payment_type = 1
//! ```

#![forbid(unsafe_code)]

mod lex;
mod parse;

use std::fmt;

use weir_domain::{Spec, SpecError};

pub use parse::SyntaxError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Syntax(SyntaxError),
    Spec(Vec<SpecError>),
}

pub fn parse(src: &str) -> Result<Spec, Error> {
    let (dims, facts) = parse::parse(src).map_err(Error::Syntax)?;
    Spec::new(dims, facts).map_err(Error::Spec)
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax(e) => write!(f, "{e}"),
            Error::Spec(errors) => {
                for (i, e) in errors.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    write!(f, "{e}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {}
