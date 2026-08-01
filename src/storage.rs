//! Typed, nullable columnar storage used by query-independent ingestion APIs.

use std::error::Error;
use std::fmt;

/// A physical column type supported by RustHouse's initial storage layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int64 => f.write_str("Int64"),
            Self::Float64 => f.write_str("Float64"),
            Self::Bool => f.write_str("Bool"),
            Self::String => f.write_str("String"),
        }
    }
}

/// One named column in a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    name: String,
    data_type: DataType,
    nullable: bool,
}

impl Field {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn data_type(&self) -> DataType {
        self.data_type
    }

    pub fn is_nullable(&self) -> bool {
        self.nullable
    }
}

/// Errors raised while constructing schemas or changing columnar storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    EmptySchema,
    EmptyFieldName {
        index: usize,
    },
    DuplicateField {
        name: String,
    },
    ColumnCount {
        expected: usize,
        actual: usize,
    },
    ColumnType {
        index: usize,
        expected: DataType,
        actual: DataType,
    },
    ColumnLength {
        index: usize,
        expected: usize,
        actual: usize,
    },
    NullInNonNullable {
        column: String,
        row: usize,
    },
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchema => f.write_str("a schema must contain at least one field"),
            Self::EmptyFieldName { index } => write!(f, "schema field {index} has an empty name"),
            Self::DuplicateField { name } => write!(f, "schema field {name:?} is duplicated"),
            Self::ColumnCount { expected, actual } => {
                write!(f, "expected {expected} columns, found {actual}")
            }
            Self::ColumnType {
                index,
                expected,
                actual,
            } => write!(f, "column {index} has type {actual}, expected {expected}"),
            Self::ColumnLength {
                index,
                expected,
                actual,
            } => write!(f, "column {index} has {actual} rows, expected {expected}"),
            Self::NullInNonNullable { column, row } => {
                write!(f, "NULL in non-nullable column {column:?} at row {row}")
            }
        }
    }
}

impl Error for StorageError {}

/// An ordered, uniquely named collection of typed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Result<Self, StorageError> {
        if fields.is_empty() {
            return Err(StorageError::EmptySchema);
        }
        for (index, field) in fields.iter().enumerate() {
            if field.name.is_empty() {
                return Err(StorageError::EmptyFieldName { index });
            }
            if fields[..index]
                .iter()
                .any(|previous| previous.name == field.name)
            {
                return Err(StorageError::DuplicateField {
                    name: field.name.clone(),
                });
            }
        }
        Ok(Self { fields })
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn field(&self, index: usize) -> Option<&Field> {
        self.fields.get(index)
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|field| field.name == name)
    }
}

/// A concrete typed column. `None` is SQL `NULL`; empty strings remain values.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Int64(Vec<Option<i64>>),
    Float64(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    String(Vec<Option<String>>),
}

impl Column {
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_null(&self, row: usize) -> Option<bool> {
        match self {
            Self::Int64(values) => values.get(row).map(Option::is_none),
            Self::Float64(values) => values.get(row).map(Option::is_none),
            Self::Bool(values) => values.get(row).map(Option::is_none),
            Self::String(values) => values.get(row).map(Option::is_none),
        }
    }

    pub(crate) fn empty(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    fn has_null(&self) -> Option<usize> {
        match self {
            Self::Int64(values) => values.iter().position(Option::is_none),
            Self::Float64(values) => values.iter().position(Option::is_none),
            Self::Bool(values) => values.iter().position(Option::is_none),
            Self::String(values) => values.iter().position(Option::is_none),
        }
    }

    fn extend_from(&mut self, other: &Self) {
        match (self, other) {
            (Self::Int64(target), Self::Int64(source)) => target.extend_from_slice(source),
            (Self::Float64(target), Self::Float64(source)) => target.extend_from_slice(source),
            (Self::Bool(target), Self::Bool(source)) => target.extend_from_slice(source),
            (Self::String(target), Self::String(source)) => target.extend(source.iter().cloned()),
            _ => unreachable!("column types are validated before append"),
        }
    }

    pub(crate) fn truncate(&mut self, rows: usize) {
        match self {
            Self::Int64(values) => values.truncate(rows),
            Self::Float64(values) => values.truncate(rows),
            Self::Bool(values) => values.truncate(rows),
            Self::String(values) => values.truncate(rows),
        }
    }
}

/// A rectangular group of typed columns.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnBatch {
    columns: Vec<Column>,
    rows: usize,
}

impl ColumnBatch {
    pub fn new(schema: &Schema, columns: Vec<Column>) -> Result<Self, StorageError> {
        let rows = Self::validate(schema, &columns)?;
        Ok(Self { columns, rows })
    }

    pub(crate) fn validate(schema: &Schema, columns: &[Column]) -> Result<usize, StorageError> {
        if columns.len() != schema.len() {
            return Err(StorageError::ColumnCount {
                expected: schema.len(),
                actual: columns.len(),
            });
        }
        let rows = columns.first().map_or(0, Column::len);
        for (index, (column, field)) in columns.iter().zip(schema.fields()).enumerate() {
            if column.data_type() != field.data_type() {
                return Err(StorageError::ColumnType {
                    index,
                    expected: field.data_type(),
                    actual: column.data_type(),
                });
            }
            if column.len() != rows {
                return Err(StorageError::ColumnLength {
                    index,
                    expected: rows,
                    actual: column.len(),
                });
            }
            if !field.is_nullable()
                && let Some(row) = column.has_null()
            {
                return Err(StorageError::NullInNonNullable {
                    column: field.name().to_owned(),
                    row,
                });
            }
        }
        Ok(rows)
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }
}

/// A schema-bound in-memory table used as the destination for bulk ingestion.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    rows: usize,
}

impl Table {
    pub fn new(schema: Schema) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|field| Column::empty(field.data_type()))
            .collect();
        Self {
            schema,
            columns,
            rows: 0,
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn append_batch(&mut self, batch: &ColumnBatch) -> Result<(), StorageError> {
        // Revalidate against this table because batches intentionally do not retain a schema.
        ColumnBatch::validate(&self.schema, batch.columns())?;
        for (target, source) in self.columns.iter_mut().zip(batch.columns()) {
            target.extend_from(source);
        }
        self.rows += batch.rows();
        Ok(())
    }

    pub(crate) fn truncate(&mut self, rows: usize) {
        for column in &mut self.columns {
            column.truncate(rows);
        }
        self.rows = rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_batch_shape_and_nullability() {
        let schema = Schema::new(vec![Field::new("id", DataType::Int64, false)]).unwrap();
        assert_eq!(
            ColumnBatch::new(&schema, vec![Column::Int64(vec![None])]),
            Err(StorageError::NullInNonNullable {
                column: "id".to_owned(),
                row: 0
            })
        );
        assert!(ColumnBatch::new(&schema, vec![Column::Bool(vec![Some(true)])]).is_err());
    }
}
