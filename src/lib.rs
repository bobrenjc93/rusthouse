//! RustHouse is an experimental, compact analytical database.

mod input;
mod sql;

pub mod output;

pub use input::{InputError, MAX_SQL_INPUT_BYTES, read_sql_input};
pub use sql::{Column, QueryResult, ScalarValue, SqlError, SqlErrorKind, execute_batch};

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
