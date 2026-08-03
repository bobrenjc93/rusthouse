//! RustHouse is an experimental, compact analytical database.

mod input;
mod sql;

pub mod output;
pub mod storage;

pub use input::{InputError, MAX_SQL_INPUT_BYTES, read_sql_input};
pub use sql::{
    MAX_BATCH_STATEMENTS, MAX_BATCH_TOKENS, MAX_SELECT_PROJECTIONS, QueryResult, ResultColumn,
    SqlError, SqlErrorKind, execute_batch,
};
pub use storage::{
    Column, ColumnSchema, DataType, InsertError, InsertRowsError, Schema, SchemaError, Table,
    Value, ValueRef,
};

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
