//! A compact, in-memory analytical SQL database.
//!
//! [`Database`] is the main embedding API. It accepts one or more SQL
//! statements and returns a result for each statement in input order.

mod csv;
mod database;
mod error;
mod sql;
mod storage;
mod value;

pub use csv::write_csv;
pub use database::{Database, ExecutionResult, Limits, QueryResult};
pub use error::{DatabaseError, LimitKind};
pub use storage::{ColumnDefinition, Schema};
pub use value::{DataType, Value};

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}
