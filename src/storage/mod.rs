//! Durable columnar storage primitives.

mod bulk;
pub mod segment;

mod table;

pub use crate::value::{DataType, Value};
pub use bulk::{Column, ColumnBatch, Field, Schema, StorageError, Table};
pub use table::ColumnDef;
pub(crate) use table::{ColumnData, Table as EngineTable};

impl From<&ColumnDef> for Field {
    fn from(column: &ColumnDef) -> Self {
        Self::new(&column.name, column.data_type, column.nullable)
    }
}

impl TryFrom<&[ColumnDef]> for Schema {
    type Error = StorageError;

    fn try_from(columns: &[ColumnDef]) -> Result<Self, Self::Error> {
        Self::new(columns.iter().map(Field::from).collect())
    }
}

impl From<&Field> for ColumnDef {
    fn from(field: &Field) -> Self {
        Self::new(field.name(), field.data_type(), field.is_nullable())
    }
}

impl Schema {
    /// Converts this format schema into durable SQL column definitions.
    pub fn to_column_defs(&self) -> Vec<ColumnDef> {
        self.fields().iter().map(ColumnDef::from).collect()
    }
}
