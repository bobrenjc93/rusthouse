//! RustHouse is a compact, dependency-free, in-memory analytical database.
//!
//! [`Database`] is the single-owner entry point, and [`SharedDatabase`] is a
//! cloneable thread-safe handle. Both accept a small SQL dialect and return
//! structured results that can be rendered by the format module.

pub mod catalog;
pub mod engine;
pub mod error;
pub mod format;
pub mod sql;
pub mod storage;
pub mod value;

pub use engine::{Database, QueryResult, ResultColumn, SharedDatabase, StatementResult};
pub use error::{Error, Result};
pub use value::{DataType, Value};

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}
