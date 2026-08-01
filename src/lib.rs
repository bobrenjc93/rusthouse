//! RustHouse is an experimental, compact analytical database.
//!
//! The scalar subsystem provides nullable SQL values, expression parsing and
//! evaluation, and NULL-aware aggregate states. It is independent of storage
//! so scans and constant-expression queries can share exactly the same rules.

mod aggregate;
mod error;
mod expression;
mod value;

pub use aggregate::{Aggregate, AggregateFunction};
pub use error::{Error, Result};
pub use expression::{BinaryOperator, EvaluationContext, Expr, UnaryOperator, evaluate, parse};
pub use value::{DataType, Value};

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_database() {
        assert_eq!(product_name(), "RustHouse");
    }
}
