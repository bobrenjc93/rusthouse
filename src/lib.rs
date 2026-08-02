//! RustHouse is an experimental, compact analytical database.
//!
//! Its storage foundation is a bounded, typed, column-major [`Table`].

pub mod column;
pub mod scalar;
pub mod schema;
pub mod table;

pub use column::{Column, ColumnError};
pub use scalar::{DataType, Scalar, ScalarValue};
pub use schema::{Field, Schema, SchemaError};
pub use table::{InsertError, Table};

/// Returns the product name while the first storage engine is being built.
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
