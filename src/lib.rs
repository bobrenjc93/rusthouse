//! RustHouse is a compact, dependency-free, in-memory analytical database.
//!
//! [`Database`] is the main entry point. It accepts a small SQL dialect and
//! can collect structured results or emit bounded row batches to a sink.

pub mod catalog;
pub mod engine;
pub mod error;
pub mod format;
pub mod sql;
pub mod storage;
pub mod value;

pub use engine::{
    Database, QueryResult, ROW_BATCH_SIZE, ResultColumn, RowBatch, RowBatchSink, StatementResult,
    StreamError,
};
pub use error::{Error, Result};
pub use value::{DataType, Value};

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}
