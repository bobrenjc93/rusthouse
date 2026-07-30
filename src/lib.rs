#![warn(missing_docs)]

//! RustHouse is a compact, dependency-free, in-memory analytical database.
//!
//! [`Database`] is the main entry point. It retains its catalog for the
//! lifetime of the value, accepts a small SQL dialect, and returns owned
//! results that can outlive the database and be rendered by [`mod@format`].
//!
//! # Execute SQL
//!
//! A batch is parsed completely before execution. Once parsing succeeds,
//! statements run in order and successful statements remain applied if a
//! later statement fails.
//!
//! ```
//! use rusthouse::{Database, StatementResult, Value};
//!
//! let mut database = Database::new();
//! let results = database.execute(
//!     "CREATE TABLE events (id Int64, label String);
//!      INSERT INTO events VALUES (2, 'second'), (1, 'first');
//!      SELECT label FROM events ORDER BY label;",
//! )?;
//!
//! assert_eq!(results.len(), 3);
//! let StatementResult::Query(query) = &results[2] else {
//!     panic!("the final statement is a SELECT");
//! };
//! assert_eq!(
//!     query.rows,
//!     vec![
//!         vec![Value::String("first".to_owned())],
//!         vec![Value::String("second".to_owned())],
//!     ],
//! );
//!
//! # Ok::<(), rusthouse::Error>(())
//! ```
//!
//! # Render a query
//!
//! Rendering consumes neither the result nor the database. The returned
//! string owns all of its output.
//!
//! ```
//! use rusthouse::format::{render, OutputFormat};
//! use rusthouse::{Database, StatementResult};
//!
//! let mut database = Database::new();
//! let results = database.execute(
//!     "CREATE TABLE totals (name String, amount Int64);
//!      INSERT INTO totals VALUES ('west', 17);
//!      SELECT name, amount FROM totals;",
//! )?;
//! let StatementResult::Query(query) = &results[2] else {
//!     panic!("the final statement is a SELECT");
//! };
//!
//! assert_eq!(render(query, OutputFormat::Csv), "name,amount\nwest,17\n");
//!
//! # Ok::<(), rusthouse::Error>(())
//! ```

/// Case-insensitive table ownership and lookup.
pub mod catalog;
/// SQL execution and owned result types.
pub mod engine;
/// Errors shared by parsing, storage, and execution.
pub mod error;
/// Deterministic table, CSV, and JSON result rendering.
pub mod format;
/// SQL syntax tree types and batch parsing.
pub mod sql;
/// Typed columnar table storage.
pub mod storage;
/// Scalar types and values.
pub mod value;

pub use engine::{Database, QueryResult, ResultColumn, StatementResult};
pub use error::{Error, Result};
pub use value::{DataType, Value};

/// Returns the static product name, `"RustHouse"`.
///
/// This function is infallible, performs no allocation or mutation, and the
/// returned string is valid for the remainder of the process.
pub fn product_name() -> &'static str {
    "RustHouse"
}
