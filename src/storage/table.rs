use std::error::Error;
use std::fmt;

use super::{DataType, Schema};

/// An owned scalar value that can be appended to a table row.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer value.
    Int64(i64),
    /// An IEEE 754 double-precision value.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// An owned UTF-8 string value.
    String(String),
}

impl Value {
    /// Returns the physical type of this value.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Int64(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Float64(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

/// A contiguous vector whose Rust element type matches its schema type.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnVector {
    /// Values for an [`DataType::Int64`] column.
    Int64(Vec<i64>),
    /// Values for a [`DataType::Float64`] column.
    Float64(Vec<f64>),
    /// Values for a [`DataType::Bool`] column.
    Bool(Vec<bool>),
    /// Values for a [`DataType::String`] column.
    String(Vec<String>),
}

impl ColumnVector {
    fn empty(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    /// Returns the physical type stored by this vector.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of stored values.
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    /// Returns whether this vector has no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the values when this is an `Int64` vector.
    pub fn as_int64(&self) -> Option<&[i64]> {
        match self {
            Self::Int64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the values when this is a `Float64` vector.
    pub fn as_float64(&self) -> Option<&[f64]> {
        match self {
            Self::Float64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the values when this is a `Bool` vector.
    pub fn as_bool(&self) -> Option<&[bool]> {
        match self {
            Self::Bool(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the values when this is a `String` vector.
    pub fn as_string(&self) -> Option<&[String]> {
        match self {
            Self::String(values) => Some(values),
            _ => None,
        }
    }

    fn push_validated(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("value types are validated before columns are mutated"),
        }
    }
}

/// An in-memory table that stores each schema field in a separate typed vector.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<ColumnVector>,
    row_count: usize,
}

impl Table {
    /// Creates an empty table with one typed vector per schema column.
    pub fn new(schema: Schema) -> Self {
        let columns = schema
            .columns()
            .iter()
            .map(|column| ColumnVector::empty(column.data_type()))
            .collect();

        Self {
            schema,
            columns,
            row_count: 0,
        }
    }

    /// Returns the table schema.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Returns the typed column vectors in schema order.
    pub fn columns(&self) -> &[ColumnVector] {
        &self.columns
    }

    /// Returns the number of complete rows in the table.
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Validates and atomically appends one complete row.
    ///
    /// Width and every value type are checked before any column is changed. A
    /// returned error therefore leaves the row count and all vectors unchanged.
    pub fn append_row(&mut self, row: Vec<Value>) -> Result<(), TableError> {
        if row.len() != self.schema.len() {
            return Err(TableError::RowWidthMismatch {
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (column_index, (column, value)) in self.schema.columns().iter().zip(&row).enumerate() {
            let actual = value.data_type();
            let expected = column.data_type();
            if actual != expected {
                return Err(TableError::TypeMismatch {
                    column_index,
                    column_name: column.name().to_owned(),
                    expected,
                    actual,
                });
            }
        }

        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push_validated(value);
        }
        self.row_count += 1;
        Ok(())
    }
}

/// An error found while validating a row append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    /// The row does not contain exactly one value per schema column.
    RowWidthMismatch {
        /// The number of columns defined by the schema.
        expected: usize,
        /// The number of values supplied in the row.
        actual: usize,
    },
    /// A value's physical type differs from its column's schema type.
    TypeMismatch {
        /// The zero-based position of the mismatched column.
        column_index: usize,
        /// The name of the mismatched column.
        column_name: String,
        /// The physical type required by the schema.
        expected: DataType,
        /// The physical type of the supplied value.
        actual: DataType,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowWidthMismatch { expected, actual } => {
                write!(
                    formatter,
                    "row has {actual} values but schema requires {expected}"
                )
            }
            Self::TypeMismatch {
                column_index,
                column_name,
                expected,
                actual,
            } => write!(
                formatter,
                "column {column_index} ({column_name}) requires {expected}, got {actual}"
            ),
        }
    }
}

impl Error for TableError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ColumnSchema, SchemaError};

    fn all_types_schema() -> Schema {
        Schema::new(vec![
            ColumnSchema::new("id", DataType::Int64),
            ColumnSchema::new("score", DataType::Float64),
            ColumnSchema::new("active", DataType::Bool),
            ColumnSchema::new("label", DataType::String),
        ])
        .unwrap()
    }

    #[test]
    fn stores_every_supported_type_in_a_typed_column() {
        let mut table = Table::new(all_types_schema());

        table
            .append_row(vec![42.into(), 3.5.into(), true.into(), "answer".into()])
            .unwrap();

        assert_eq!(table.row_count(), 1);
        assert_eq!(table.columns()[0].as_int64(), Some(&[42][..]));
        assert_eq!(table.columns()[1].as_float64(), Some(&[3.5][..]));
        assert_eq!(table.columns()[2].as_bool(), Some(&[true][..]));
        assert_eq!(
            table.columns()[3].as_string(),
            Some(&[String::from("answer")][..])
        );
        assert!(table.columns().iter().all(|column| column.len() == 1));
    }

    #[test]
    fn rejects_duplicate_column_names() {
        let error = Schema::new(vec![
            ColumnSchema::new("value", DataType::Int64),
            ColumnSchema::new("value", DataType::String),
        ])
        .unwrap_err();

        assert_eq!(
            error,
            SchemaError::DuplicateColumnName {
                name: "value".to_owned()
            }
        );
    }

    #[test]
    fn rejects_short_and_long_rows_without_changing_the_table() {
        let mut table = Table::new(all_types_schema());
        let initial = table.clone();

        assert_eq!(
            table.append_row(vec![1.into()]),
            Err(TableError::RowWidthMismatch {
                expected: 4,
                actual: 1
            })
        );
        assert_eq!(table, initial);

        assert_eq!(
            table.append_row(vec![
                1.into(),
                2.0.into(),
                false.into(),
                "ok".into(),
                5.into()
            ]),
            Err(TableError::RowWidthMismatch {
                expected: 4,
                actual: 5
            })
        );
        assert_eq!(table, initial);
    }

    #[test]
    fn late_type_mismatch_leaves_every_column_unchanged() {
        let mut table = Table::new(all_types_schema());
        table
            .append_row(vec![1.into(), 1.5.into(), true.into(), "first".into()])
            .unwrap();
        let initial = table.clone();

        let error = table
            .append_row(vec![2.into(), 2.5.into(), false.into(), 99.into()])
            .unwrap_err();

        assert_eq!(
            error,
            TableError::TypeMismatch {
                column_index: 3,
                column_name: "label".to_owned(),
                expected: DataType::String,
                actual: DataType::Int64,
            }
        );
        assert_eq!(table, initial);
        assert_eq!(table.row_count(), 1);
    }

    #[test]
    fn reports_type_mismatches_for_every_schema_type() {
        let cases = [
            (DataType::Int64, Value::Bool(false)),
            (DataType::Float64, Value::Int64(1)),
            (DataType::Bool, Value::String("false".to_owned())),
            (DataType::String, Value::Float64(1.0)),
        ];

        for (expected, value) in cases {
            let schema = Schema::new(vec![ColumnSchema::new("value", expected)]).unwrap();
            let mut table = Table::new(schema);
            let actual = value.data_type();

            assert_eq!(
                table.append_row(vec![value]),
                Err(TableError::TypeMismatch {
                    column_index: 0,
                    column_name: "value".to_owned(),
                    expected,
                    actual,
                })
            );
            assert_eq!(table.row_count(), 0);
            assert!(table.columns()[0].is_empty());
        }
    }
}
