//! Durable columnar storage primitives.

mod bulk;
pub mod segment;

mod table;

pub use bulk::{Column, ColumnBatch, Field, Schema, StorageError, Table};
pub(crate) use table::{ColumnData, Table as EngineTable};
pub use table::{ColumnDef, DataType, Value};
