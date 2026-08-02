//! RustHouse is an experimental, compact analytical database.

pub mod storage;

pub use storage::{
    BoolColumn, Column, ColumnSchema, DataType, Float64Column, InsertError, Int64Column,
    NonFiniteFloat, Schema, SchemaError, StringColumn, Table, TableLimits, TypedColumn, Value,
    ValueRef, ValueType,
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
