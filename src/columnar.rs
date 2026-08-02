//! Typed, in-memory columnar storage.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

/// The logical type of values stored in a [`Column`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        };
        formatter.write_str(name)
    }
}

/// A contiguous, native representation of values of one logical type.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    /// Returns the logical type shared by all values in this column.
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of rows in this column.
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    /// Returns `true` when this column contains no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A column and its schema name.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedColumn {
    name: String,
    column: Column,
}

impl NamedColumn {
    pub fn new(name: impl Into<String>, column: Column) -> Self {
        Self {
            name: name.into(),
            column,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn column(&self) -> &Column {
        &self.column
    }
}

/// A set of named columns with a shared row count.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordBatch {
    columns: Vec<NamedColumn>,
    row_count: usize,
}

impl RecordBatch {
    /// Creates a batch after validating its schema and column lengths.
    ///
    /// A batch with no columns has zero rows. Columns with empty vectors form a
    /// valid zero-row batch.
    pub fn try_new(columns: Vec<NamedColumn>) -> Result<Self, RecordBatchError> {
        let mut names = HashMap::with_capacity(columns.len());
        for (index, named_column) in columns.iter().enumerate() {
            if named_column.name.is_empty() {
                return Err(RecordBatchError::EmptyColumnName { index });
            }

            if let Some(first_index) = names.insert(named_column.name.as_str(), index) {
                return Err(RecordBatchError::DuplicateColumnName {
                    name: named_column.name.clone(),
                    first_index,
                    duplicate_index: index,
                });
            }
        }

        let row_count = columns.first().map_or(0, |column| column.column.len());
        for (index, named_column) in columns.iter().enumerate().skip(1) {
            let actual = named_column.column.len();
            if actual != row_count {
                return Err(RecordBatchError::ColumnLengthMismatch {
                    name: named_column.name.clone(),
                    index,
                    expected: row_count,
                    actual,
                });
            }
        }

        Ok(Self { columns, row_count })
    }

    /// Returns the number of rows shared by every column.
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the number of columns in the batch.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Returns `true` when the batch contains no rows.
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Returns the named columns in schema order.
    pub fn columns(&self) -> &[NamedColumn] {
        &self.columns
    }

    /// Finds a column by its unique schema name.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns
            .iter()
            .find(|column| column.name == name)
            .map(NamedColumn::column)
    }
}

/// A schema or row-shape violation found while constructing a [`RecordBatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordBatchError {
    EmptyColumnName {
        index: usize,
    },
    DuplicateColumnName {
        name: String,
        first_index: usize,
        duplicate_index: usize,
    },
    ColumnLengthMismatch {
        name: String,
        index: usize,
        expected: usize,
        actual: usize,
    },
}

impl fmt::Display for RecordBatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyColumnName { index } => {
                write!(formatter, "column at index {index} has an empty name")
            }
            Self::DuplicateColumnName {
                name,
                first_index,
                duplicate_index,
            } => write!(
                formatter,
                "column name `{name}` at index {duplicate_index} duplicates index {first_index}"
            ),
            Self::ColumnLengthMismatch {
                name,
                index,
                expected,
                actual,
            } => write!(
                formatter,
                "column `{name}` at index {index} has {actual} rows, expected {expected}"
            ),
        }
    }
}

impl Error for RecordBatchError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_column_type_reports_its_type_and_length() {
        let columns = [
            (Column::Int64(vec![1, 2]), DataType::Int64, 2),
            (Column::Float64(vec![1.5]), DataType::Float64, 1),
            (Column::Bool(vec![true, false, true]), DataType::Bool, 3),
            (
                Column::String(vec!["rust".to_owned(), "house".to_owned()]),
                DataType::String,
                2,
            ),
        ];

        for (column, expected_type, expected_len) in columns {
            assert_eq!(column.data_type(), expected_type);
            assert_eq!(column.len(), expected_len);
            assert!(!column.is_empty());
        }
    }

    #[test]
    fn record_batch_preserves_typed_columns_and_schema_order() {
        let batch = RecordBatch::try_new(vec![
            NamedColumn::new("id", Column::Int64(vec![1, 2])),
            NamedColumn::new("score", Column::Float64(vec![2.5, 4.0])),
            NamedColumn::new("active", Column::Bool(vec![true, false])),
            NamedColumn::new(
                "label",
                Column::String(vec!["a".to_owned(), "b".to_owned()]),
            ),
        ])
        .unwrap();

        assert_eq!(batch.row_count(), 2);
        assert_eq!(batch.column_count(), 4);
        assert_eq!(batch.columns()[0].name(), "id");
        assert_eq!(
            batch.column("score").unwrap().data_type(),
            DataType::Float64
        );
        assert_eq!(batch.column("missing"), None);
    }

    #[test]
    fn zero_row_batches_are_valid() {
        let batch = RecordBatch::try_new(vec![
            NamedColumn::new("id", Column::Int64(vec![])),
            NamedColumn::new("name", Column::String(vec![])),
        ])
        .unwrap();

        assert_eq!(batch.row_count(), 0);
        assert_eq!(batch.column_count(), 2);
        assert!(batch.is_empty());

        let schemaless = RecordBatch::try_new(vec![]).unwrap();
        assert_eq!(schemaless.row_count(), 0);
        assert_eq!(schemaless.column_count(), 0);
    }

    #[test]
    fn empty_column_names_are_rejected() {
        let error =
            RecordBatch::try_new(vec![NamedColumn::new("", Column::Bool(vec![]))]).unwrap_err();

        assert_eq!(error, RecordBatchError::EmptyColumnName { index: 0 });
    }

    #[test]
    fn duplicate_column_names_are_rejected() {
        let error = RecordBatch::try_new(vec![
            NamedColumn::new("id", Column::Int64(vec![1])),
            NamedColumn::new("id", Column::Int64(vec![2])),
        ])
        .unwrap_err();

        assert_eq!(
            error,
            RecordBatchError::DuplicateColumnName {
                name: "id".to_owned(),
                first_index: 0,
                duplicate_index: 1,
            }
        );
    }

    #[test]
    fn mismatched_column_lengths_are_rejected() {
        let error = RecordBatch::try_new(vec![
            NamedColumn::new("id", Column::Int64(vec![1, 2])),
            NamedColumn::new("active", Column::Bool(vec![true])),
        ])
        .unwrap_err();

        assert_eq!(
            error,
            RecordBatchError::ColumnLengthMismatch {
                name: "active".to_owned(),
                index: 1,
                expected: 2,
                actual: 1,
            }
        );
    }
}
