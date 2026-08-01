//! RustHouse is a compact, in-memory columnar analytical database.
//!
//! [`Engine`] owns an isolated catalog. It accepts a semicolon-separated SQL
//! batch and returns a result for every statement.

mod engine;
mod error;
pub mod output;
mod storage;
mod types;

pub use engine::{Engine, EngineConfig, QueryResult, StatementResult};
pub use error::{Error, Result};
pub use storage::{ColumnVector, Field, Schema, Table};
pub use types::{DataType, Value};

pub fn product_name() -> &'static str {
    "RustHouse"
}
