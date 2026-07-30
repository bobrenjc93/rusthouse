//! A compact, dependency-free, in-memory analytical database.
//!
//! [`Database`] is the main entry point. It accepts semicolon-separated SQL
//! statements and returns structured [`StatementResult`] values.
//!
//! # Create, insert, and select
//!
//! ```
//! use rusthouse::{Database, StatementResult, Value};
//!
//! let mut database = Database::new();
//! let results = database.execute(
//!     "CREATE TABLE events (id Int64, name String);
//!      INSERT INTO events VALUES (2, 'deploy'), (1, 'build');
//!      SELECT id, name FROM events ORDER BY id;",
//! )?;
//!
//! assert_eq!(results.len(), 3);
//! assert!(matches!(
//!     results[1],
//!     StatementResult::Command {
//!         tag: "INSERT",
//!         affected_rows: 2,
//!     }
//! ));
//! let StatementResult::Query(query) = &results[2] else {
//!     panic!("expected SELECT result");
//! };
//! assert_eq!(
//!     query.rows,
//!     vec![
//!         vec![Value::Int64(1), Value::String("build".to_owned())],
//!         vec![Value::Int64(2), Value::String("deploy".to_owned())],
//!     ]
//! );
//! # Ok::<(), rusthouse::Error>(())
//! ```
#![warn(missing_docs)]

/// Case-insensitive table catalog and lookup operations.
pub mod catalog;
/// SQL execution and structured statement results.
pub mod engine;
/// Errors returned by parsing, storage, and execution.
pub mod error;
/// Table, CSV, and JSON result rendering.
pub mod format;
/// SQL syntax tree and parser.
pub mod sql;
/// Typed columnar table storage.
pub mod storage;
/// SQL data types and owned scalar values.
pub mod value;

pub use engine::{Database, QueryResult, ResultColumn, StatementResult};
pub use error::{Error, Result};
pub use value::{DataType, Value};

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}
