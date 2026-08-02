//! Typed, bounded, in-memory columnar storage.

use std::error::Error;
use std::fmt;

/// A scalar type supported by the storage layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// The name and type of one column in a [`Schema`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
}

impl ColumnSchema {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// An ordered collection of column definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    columns: Vec<ColumnSchema>,
}

impl Schema {
    pub fn new(columns: Vec<ColumnSchema>) -> Self {
        Self { columns }
    }

    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.columns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// One owned scalar value supplied to an insert.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Value {
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }
}

/// One logical row in a batch insert.
pub type Row = Vec<Value>;

/// A physical, homogeneous column vector.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

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

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("batch values are type-checked before insertion"),
        }
    }
}

/// A validation failure that leaves the table's logical contents unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageError {
    CapacityExceeded {
        capacity: usize,
        current_rows: usize,
        batch_rows: usize,
    },
    RowWidth {
        row: usize,
        expected: usize,
        actual: usize,
    },
    TypeMismatch {
        row: usize,
        column: usize,
        expected: DataType,
        actual: DataType,
    },
    NonFiniteFloat {
        row: usize,
        column: usize,
    },
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExceeded {
                capacity,
                current_rows,
                batch_rows,
            } => write!(
                formatter,
                "inserting {batch_rows} rows into a table with {current_rows} rows exceeds its capacity of {capacity}"
            ),
            Self::RowWidth {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "row {row} has {actual} values, but the schema requires {expected}"
            ),
            Self::TypeMismatch {
                row,
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "row {row}, column {column} has type {actual}, but the schema requires {expected}"
            ),
            Self::NonFiniteFloat { row, column } => {
                write!(
                    formatter,
                    "row {row}, column {column} is not a finite Float64"
                )
            }
        }
    }
}

impl Error for StorageError {}

/// A row-bounded table backed by one physical vector per schema column.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
    capacity: usize,
}

impl Table {
    pub fn new(schema: Schema, capacity: usize) -> Self {
        let columns = schema
            .columns()
            .iter()
            .map(|column| Column::new(column.data_type()))
            .collect();
        Self {
            schema,
            columns,
            row_count: 0,
            capacity,
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.row_count
    }

    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Validates and atomically appends a batch of logical rows.
    ///
    /// Capacity, row widths, value types, and float finiteness are checked for
    /// every row before any physical column is changed.
    pub fn insert_batch(&mut self, rows: Vec<Row>) -> Result<(), StorageError> {
        self.validate_batch(&rows)?;

        let inserted_rows = rows.len();
        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count += inserted_rows;
        Ok(())
    }

    fn validate_batch(&self, rows: &[Row]) -> Result<(), StorageError> {
        let fits = self
            .row_count
            .checked_add(rows.len())
            .is_some_and(|new_len| new_len <= self.capacity);
        if !fits {
            return Err(StorageError::CapacityExceeded {
                capacity: self.capacity,
                current_rows: self.row_count,
                batch_rows: rows.len(),
            });
        }

        for (row_index, row) in rows.iter().enumerate() {
            if row.len() != self.schema.len() {
                return Err(StorageError::RowWidth {
                    row: row_index,
                    expected: self.schema.len(),
                    actual: row.len(),
                });
            }

            for (column_index, (value, column)) in row.iter().zip(self.schema.columns()).enumerate()
            {
                let actual = value.data_type();
                let expected = column.data_type();
                if actual != expected {
                    return Err(StorageError::TypeMismatch {
                        row: row_index,
                        column: column_index,
                        expected,
                        actual,
                    });
                }
                if let Value::Float64(value) = value {
                    if !value.is_finite() {
                        return Err(StorageError::NonFiniteFloat {
                            row: row_index,
                            column: column_index,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn four_type_schema() -> Schema {
        Schema::new(vec![
            ColumnSchema::new("signed", DataType::Int64),
            ColumnSchema::new("ratio", DataType::Float64),
            ColumnSchema::new("enabled", DataType::Bool),
            ColumnSchema::new("label", DataType::String),
        ])
    }

    fn row(integer: i64, float: f64, boolean: bool, string: &str) -> Row {
        vec![
            Value::Int64(integer),
            Value::Float64(float),
            Value::Bool(boolean),
            Value::String(string.to_owned()),
        ]
    }

    #[test]
    fn stores_all_supported_types_in_physical_columns() {
        let mut table = Table::new(four_type_schema(), 3);

        table
            .insert_batch(vec![
                row(-7, 1.25, true, "first"),
                row(9, -2.5, false, "second"),
            ])
            .unwrap();

        assert_eq!(table.len(), 2);
        assert_eq!(table.capacity(), 3);
        assert_eq!(table.schema().columns()[0].name(), "signed");
        assert_eq!(
            table.columns(),
            &[
                Column::Int64(vec![-7, 9]),
                Column::Float64(vec![1.25, -2.5]),
                Column::Bool(vec![true, false]),
                Column::String(vec!["first".to_owned(), "second".to_owned()]),
            ]
        );
    }

    #[test]
    fn invalid_middle_row_rolls_back_the_whole_batch() {
        let mut table = Table::new(four_type_schema(), 5);
        table
            .insert_batch(vec![row(1, 1.0, true, "existing")])
            .unwrap();
        let before = table.clone();

        let error = table
            .insert_batch(vec![
                row(2, 2.0, false, "valid"),
                vec![
                    Value::Int64(3),
                    Value::Float64(3.0),
                    Value::String("not a bool".to_owned()),
                    Value::String("invalid".to_owned()),
                ],
                row(4, 4.0, true, "never inserted"),
            ])
            .unwrap_err();

        assert_eq!(
            error,
            StorageError::TypeMismatch {
                row: 1,
                column: 2,
                expected: DataType::Bool,
                actual: DataType::String,
            }
        );
        assert_eq!(table, before);
    }

    #[test]
    fn rejects_bad_row_width_without_mutation() {
        let mut table = Table::new(four_type_schema(), 2);

        let error = table.insert_batch(vec![vec![Value::Int64(1)]]).unwrap_err();

        assert_eq!(
            error,
            StorageError::RowWidth {
                row: 0,
                expected: 4,
                actual: 1,
            }
        );
        assert!(table.is_empty());
        assert!(table.columns().iter().all(Column::is_empty));
    }

    #[test]
    fn rejects_non_finite_floats_without_mutation() {
        for non_finite in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut table = Table::new(four_type_schema(), 1);

            let error = table
                .insert_batch(vec![row(1, non_finite, true, "invalid")])
                .unwrap_err();

            assert_eq!(StorageError::NonFiniteFloat { row: 0, column: 1 }, error);
            assert!(table.is_empty());
        }
    }

    #[test]
    fn rejects_batch_that_exceeds_remaining_capacity() {
        let mut table = Table::new(four_type_schema(), 2);
        table
            .insert_batch(vec![row(1, 1.0, true, "existing")])
            .unwrap();
        let before = table.clone();

        let error = table
            .insert_batch(vec![row(2, 2.0, false, "two"), row(3, 3.0, true, "three")])
            .unwrap_err();

        assert_eq!(
            error,
            StorageError::CapacityExceeded {
                capacity: 2,
                current_rows: 1,
                batch_rows: 2,
            }
        );
        assert_eq!(table, before);
    }

    #[test]
    fn reports_mismatches_for_each_supported_type() {
        let expected_types = [
            DataType::Int64,
            DataType::Float64,
            DataType::Bool,
            DataType::String,
        ];
        let wrong_values = [
            Value::Float64(1.0),
            Value::Bool(true),
            Value::String("wrong".to_owned()),
            Value::Int64(1),
        ];

        for (column, wrong_value) in wrong_values.into_iter().enumerate() {
            let mut values = row(1, 1.0, true, "valid");
            let actual = wrong_value.data_type();
            values[column] = wrong_value;
            let mut table = Table::new(four_type_schema(), 1);

            assert_eq!(
                table.insert_batch(vec![values]),
                Err(StorageError::TypeMismatch {
                    row: 0,
                    column,
                    expected: expected_types[column],
                    actual,
                })
            );
            assert!(table.is_empty());
        }
    }

    #[test]
    fn empty_batch_is_a_no_op_at_capacity() {
        let mut table = Table::new(Schema::default(), 0);

        table.insert_batch(Vec::new()).unwrap();

        assert!(table.is_empty());
        assert!(table.columns().is_empty());
    }
}
