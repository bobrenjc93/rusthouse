//! Typed, in-memory columnar table storage.
//!
//! A [`Schema`] validates column names when it is constructed. A [`Table`]
//! then uses that schema to validate complete rows before appending values to
//! its typed column vectors.

mod schema;
mod table;

pub use schema::{ColumnSchema, DataType, Schema, SchemaError};
pub use table::{ColumnVector, Table, TableError, Value};
