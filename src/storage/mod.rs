//! Durable columnar storage primitives.

pub mod segment;

mod table;

pub(crate) use table::{ColumnData, Table};
pub use table::{ColumnDef, DataType, Value};
