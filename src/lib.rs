#![deny(missing_docs)]
//! RustHouse is an experimental, compact analytical database.
//!
//! [`Database`] provides the stateful SQL interface, while [`Table`] exposes
//! lower-level typed columnar storage. Query results can be serialized with
//! [`write_csv`].
//!
//! ```
//! use rusthouse::{Database, ScalarValue};
//!
//! let mut database = Database::new();
//! let results = database.execute(
//!     "CREATE TABLE events (id Int64, note Nullable(String));
//!      INSERT INTO events VALUES (1, NULL), (2, 'ready');
//!      SELECT COUNT(*) AS event_count FROM events;",
//! )?;
//!
//! assert_eq!(results[0].header, "event_count");
//! assert_eq!(results[0].value, ScalarValue::Integer(2));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod csv;
mod database;
mod evaluator;
mod parser;
pub mod storage;

pub use csv::write_csv;
pub use database::{DEFAULT_TABLE_ROW_LIMIT, Database};
pub use parser::{
    MAX_SQL_INPUT_BYTES, MAX_SQL_STATEMENTS, QueryResult, ScalarValue, SqlError, SqlErrorKind,
    parse_sql_batch, product_name,
};
pub use storage::{
    AppendError, BatchAppendError, Column, DataType, Field, MAX_IDENTIFIER_BYTES,
    MAX_SCHEMA_FIELDS, MAX_STORED_STRING_BYTES, MAX_TABLE_DATA_BYTES, Schema, SchemaError, Table,
    TypedColumn, ValidityBitmap, Value, ValueType,
};

pub(crate) use parser::{CreateTable, DatabaseEvent, DatabaseEventOutcome, parse_database_batch};
