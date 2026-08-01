//! RustHouse is a compact, in-memory analytical SQL database.
//!
//! [`Database`] owns a session catalog. SQL scripts may contain CREATE TABLE,
//! INSERT VALUES, and SELECT statements; SELECT results can be emitted with
//! [`write_csv`] or [`CsvWriter`].

mod ast;
mod engine;
mod error;
mod format;
mod lexer;
mod parser;
mod storage;
mod value;

pub use engine::{
    Database, MAX_MATERIALIZED_RESULT_BYTES, MAX_RESULT_ROWS, QueryResult, ResultColumn,
};
pub use error::{Error, Result};
pub use format::{CsvWriter, MAX_OUTPUT_BYTES, write_csv};
pub use storage::{ColumnSchema, MAX_COLUMNS, MAX_ROWS_PER_TABLE, MAX_TABLES};
pub use value::{DataType, Value};

pub const MAX_INPUT_BYTES: usize = 128 * 1024 * 1024;

pub fn product_name() -> &'static str {
    "RustHouse"
}
