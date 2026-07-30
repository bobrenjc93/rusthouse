//! RustHouse is a compact, dependency-free, in-memory analytical database.
//!
//! Database is the main entry point. It accepts a small SQL dialect and
//! returns structured results that can be rendered by the format module.

pub mod catalog;
pub mod engine;
pub mod error;
pub mod format;
pub mod sql;
pub mod storage;
pub mod value;

pub use engine::{
    DEFAULT_MAX_IN_MEMORY_GROUPS, Database, DatabaseOptions, QueryResult, ResultColumn,
    StatementResult,
};
pub use error::{Error, Result};
pub use value::{DataType, Value};

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}
