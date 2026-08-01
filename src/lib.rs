//! RustHouse is a compact analytical database with snapshot-isolated sessions.

mod catalog;
mod database;
mod error;
mod persistence;
mod sql;
pub mod storage;

pub use database::{Database, ResultSet, Session, StatementResult, TransactionLimits};
pub use error::{Error, LimitKind, Result};
pub use storage::{ColumnDef, DataType, Value};

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
