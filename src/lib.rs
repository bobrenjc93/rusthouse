//! RustHouse is a compact, dependency-free, in-memory analytical database.
//!
//! [`Database`] is the single-owner entry point. [`SharedDatabase`] adds a
//! cloneable, synchronized handle for concurrent query execution. Both accept
//! a small SQL dialect and return structured results that can be rendered by
//! the [`mod@format`] module.

pub mod catalog;
pub mod engine;
pub mod error;
pub mod format;
pub mod shared;
pub mod sql;
pub mod storage;
pub mod value;

pub use engine::{Database, QueryResult, ResultColumn, StatementResult};
pub use error::{Error, LockAccess, Result};
pub use shared::SharedDatabase;
pub use value::{DataType, Value};

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}
