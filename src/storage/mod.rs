//! Durable columnar storage primitives.

mod bulk;
pub mod segment;

mod table;

pub use crate::value::{DataType, Value};
pub use bulk::{Column, ColumnBatch, Field, Schema, StorageError, Table};
pub use table::ColumnDef;
pub(crate) use table::{ColumnData, Table as EngineTable};
