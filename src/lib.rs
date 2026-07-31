#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
//! RustHouse is a compact, dependency-free, in-memory analytical database.
//!
//! [`Database`] is the main entry point. It accepts a small SQL dialect and
//! returns structured results that can be rendered by the [`mod@format`] module.
//!
//! # End-to-end example
//!
//! The database retains its in-memory catalog across calls to
//! [`Database::execute`]. SQL batches return one [`StatementResult`] per
//! statement.
//!
//! ```rust
//! use rusthouse::{DataType, Database, StatementResult, Value};
//!
//! let mut database = Database::new();
//! let _ = database.execute(
//!     "CREATE TABLE sales (region String, amount Int64); \
//!      INSERT INTO sales VALUES ('west', 10), ('east', 4), ('west', 7);",
//! )?;
//!
//! let results = database.execute(
//!     "SELECT region, SUM(amount) AS total \
//!      FROM sales GROUP BY region ORDER BY total DESC",
//! )?;
//! let StatementResult::Query(query) = &results[0] else {
//!     unreachable!("SELECT always returns a query result");
//! };
//!
//! assert_eq!(query.columns[1].name, "total");
//! assert_eq!(query.columns[1].data_type, DataType::Int64);
//! assert_eq!(query.rows[0], vec![Value::String("west".into()), Value::Int64(17)]);
//!
//! # Ok::<(), rusthouse::Error>(())
//! ```

pub mod catalog;
pub mod engine;
pub mod error;
pub mod execution;
pub mod format;
pub mod sql;
pub mod storage;
pub mod value;

pub use engine::{Database, QueryResult, ResultColumn, StatementResult};
pub use error::{Error, Resource, Result};
pub use execution::{ExecutionLimits, ExecutionStats};
pub use value::{DataType, Value};

/// Returns the product name used by RustHouse front ends.
///
/// # Examples
///
/// ```
/// assert_eq!(rusthouse::product_name(), "RustHouse");
/// ```
pub fn product_name() -> &'static str {
    "RustHouse"
}
