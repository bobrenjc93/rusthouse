//! RustHouse is an experimental, compact analytical database.

pub mod storage;

pub use storage::{
    AppendError, Column, DataType, Field, Schema, SchemaError, Table, TypedColumn, ValidityBitmap,
    Value, ValueType,
};

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
