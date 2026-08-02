//! Typed, columnar in-memory storage.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// The largest number of rows accepted by one call to [`Table::insert_rows`].
///
/// Bounding individual batches prevents an accidental input from requiring an
/// unbounded temporary validation pass. Larger inputs can be split across
/// multiple calls.
pub const MAX_BATCH_ROWS: usize = 65_536;

/// A logical type supported by the in-memory storage layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int64 => formatter.write_str("Int64"),
            Self::Float64 => formatter.write_str("Float64"),
            Self::Bool => formatter.write_str("Bool"),
            Self::String => formatter.write_str("String"),
        }
    }
}

/// A named column in a [`Schema`].
#[derive(Clone, Debug, Eq, PartialEq)]
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

/// Errors produced while constructing a schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaError {
    Empty,
    DuplicateColumn { name: String },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a table schema must contain at least one column"),
            Self::DuplicateColumn { name } => {
                write!(formatter, "schema contains duplicate column {name:?}")
            }
        }
    }
}

impl Error for SchemaError {}

/// An ordered, validated collection of column definitions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schema {
    columns: Vec<ColumnSchema>,
}

impl Schema {
    pub fn new(columns: Vec<ColumnSchema>) -> Result<Self, SchemaError> {
        if columns.is_empty() {
            return Err(SchemaError::Empty);
        }

        let mut names = HashSet::with_capacity(columns.len());
        for column in &columns {
            if !names.insert(column.name()) {
                return Err(SchemaError::DuplicateColumn {
                    name: column.name.clone(),
                });
            }
        }

        Ok(Self { columns })
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

    pub fn column(&self, index: usize) -> Option<&ColumnSchema> {
        self.columns.get(index)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }
}

/// An owned cell supplied to an insertion batch.
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

/// A physical column represented by one contiguous vector of its logical type.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    fn empty(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    fn reserve(&mut self, additional: usize) {
        match self {
            Self::Int64(values) => values.reserve(additional),
            Self::Float64(values) => values.reserve(additional),
            Self::Bool(values) => values.reserve(additional),
            Self::String(values) => values.reserve(additional),
        }
    }

    fn push(&mut self, value: &Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(*value),
            (Self::Float64(values), Value::Float64(value)) => values.push(*value),
            (Self::Bool(values), Value::Bool(value)) => values.push(*value),
            (Self::String(values), Value::String(value)) => values.push(value.clone()),
            _ => unreachable!("values are type checked before columns are mutated"),
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

    pub fn as_int64(&self) -> Option<&[i64]> {
        match self {
            Self::Int64(values) => Some(values),
            _ => None,
        }
    }

    pub fn as_float64(&self) -> Option<&[f64]> {
        match self {
            Self::Float64(values) => Some(values),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<&[bool]> {
        match self {
            Self::Bool(values) => Some(values),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&[String]> {
        match self {
            Self::String(values) => Some(values),
            _ => None,
        }
    }
}

/// Errors that reject an insertion batch before any row is written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InsertError {
    BatchTooLarge {
        actual: usize,
        maximum: usize,
    },
    WrongRowWidth {
        row: usize,
        expected: usize,
        actual: usize,
    },
    TypeMismatch {
        row: usize,
        column: usize,
        column_name: String,
        expected: DataType,
        actual: DataType,
    },
    NonFiniteFloat {
        row: usize,
        column: usize,
        column_name: String,
    },
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BatchTooLarge { actual, maximum } => write!(
                formatter,
                "insertion batch has {actual} rows, exceeding the limit of {maximum}"
            ),
            Self::WrongRowWidth {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "row {row} has {actual} values but the schema requires {expected}"
            ),
            Self::TypeMismatch {
                row,
                column,
                column_name,
                expected,
                actual,
            } => write!(
                formatter,
                "row {row}, column {column} ({column_name:?}) expects {expected} but received {actual}"
            ),
            Self::NonFiniteFloat {
                row,
                column,
                column_name,
            } => write!(
                formatter,
                "row {row}, column {column} ({column_name:?}) contains a non-finite Float64"
            ),
        }
    }
}

impl Error for InsertError {}

/// A schema-backed collection of equally sized, typed columns.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    pub fn new(schema: Schema) -> Self {
        let columns = schema
            .columns()
            .iter()
            .map(|column| Column::empty(column.data_type()))
            .collect();

        Self {
            schema,
            columns,
            row_count: 0,
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    pub fn column_by_name(&self, name: &str) -> Option<&Column> {
        self.schema
            .column_index(name)
            .and_then(|index| self.columns.get(index))
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Inserts a batch after validating every row and cell.
    ///
    /// Validation completes before any column is changed, so every typed
    /// failure leaves the table exactly as it was before the call.
    pub fn insert_rows(&mut self, rows: impl AsRef<[Vec<Value>]>) -> Result<(), InsertError> {
        let rows = rows.as_ref();
        self.validate_rows(rows)?;

        for (column_index, column) in self.columns.iter_mut().enumerate() {
            column.reserve(rows.len());
            for row in rows {
                column.push(&row[column_index]);
            }
        }
        self.row_count += rows.len();

        debug_assert!(
            self.columns
                .iter()
                .all(|column| column.len() == self.row_count)
        );
        Ok(())
    }

    fn validate_rows(&self, rows: &[Vec<Value>]) -> Result<(), InsertError> {
        if rows.len() > MAX_BATCH_ROWS {
            return Err(InsertError::BatchTooLarge {
                actual: rows.len(),
                maximum: MAX_BATCH_ROWS,
            });
        }

        for (row_index, row) in rows.iter().enumerate() {
            if row.len() != self.schema.len() {
                return Err(InsertError::WrongRowWidth {
                    row: row_index,
                    expected: self.schema.len(),
                    actual: row.len(),
                });
            }

            for (column_index, (value, definition)) in
                row.iter().zip(self.schema.columns()).enumerate()
            {
                let expected = definition.data_type();
                let actual = value.data_type();
                if actual != expected {
                    return Err(InsertError::TypeMismatch {
                        row: row_index,
                        column: column_index,
                        column_name: definition.name.clone(),
                        expected,
                        actual,
                    });
                }

                if matches!(value, Value::Float64(value) if !value.is_finite()) {
                    return Err(InsertError::NonFiniteFloat {
                        row: row_index,
                        column: column_index,
                        column_name: definition.name.clone(),
                    });
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_schema() -> Schema {
        Schema::new(vec![
            ColumnSchema::new("id", DataType::Int64),
            ColumnSchema::new("score", DataType::Float64),
            ColumnSchema::new("active", DataType::Bool),
            ColumnSchema::new("label", DataType::String),
        ])
        .unwrap()
    }

    #[test]
    fn rejects_empty_and_duplicate_schemas() {
        assert_eq!(Schema::new(vec![]), Err(SchemaError::Empty));
        assert_eq!(
            Schema::new(vec![
                ColumnSchema::new("id", DataType::Int64),
                ColumnSchema::new("id", DataType::String),
            ]),
            Err(SchemaError::DuplicateColumn {
                name: "id".to_owned(),
            })
        );
    }

    #[test]
    fn inserts_multiple_rows_into_distinct_typed_columns() {
        let mut table = Table::new(full_schema());
        table
            .insert_rows(vec![
                vec![1.into(), 1.5.into(), true.into(), "alpha".into()],
                vec![2.into(), (-3.25).into(), false.into(), "beta".into()],
                vec![3.into(), 0.0.into(), true.into(), "gamma".into()],
            ])
            .unwrap();

        assert_eq!(table.row_count(), 3);
        assert_eq!(table.columns().len(), 4);
        assert_eq!(
            table.column_by_name("id").unwrap().as_int64(),
            Some([1, 2, 3].as_slice())
        );
        assert_eq!(
            table.column_by_name("score").unwrap().as_float64(),
            Some([1.5, -3.25, 0.0].as_slice())
        );
        assert_eq!(
            table.column_by_name("active").unwrap().as_bool(),
            Some([true, false, true].as_slice())
        );
        assert_eq!(
            table.column_by_name("label").unwrap().as_string(),
            Some(["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()].as_slice())
        );
    }

    #[test]
    fn rejects_invalid_rows_with_typed_errors() {
        let mut table = Table::new(full_schema());

        assert_eq!(
            table.insert_rows(vec![vec![1.into()]]),
            Err(InsertError::WrongRowWidth {
                row: 0,
                expected: 4,
                actual: 1,
            })
        );
        assert_eq!(
            table.insert_rows(vec![vec![
                1.into(),
                1.0.into(),
                Value::String("not a bool".to_owned()),
                "label".into(),
            ]]),
            Err(InsertError::TypeMismatch {
                row: 0,
                column: 2,
                column_name: "active".to_owned(),
                expected: DataType::Bool,
                actual: DataType::String,
            })
        );
        assert_eq!(
            table.insert_rows(vec![vec![
                1.into(),
                f64::INFINITY.into(),
                true.into(),
                "label".into(),
            ]]),
            Err(InsertError::NonFiniteFloat {
                row: 0,
                column: 1,
                column_name: "score".to_owned(),
            })
        );

        let oversized = vec![Vec::new(); MAX_BATCH_ROWS + 1];
        assert_eq!(
            table.insert_rows(oversized),
            Err(InsertError::BatchTooLarge {
                actual: MAX_BATCH_ROWS + 1,
                maximum: MAX_BATCH_ROWS,
            })
        );
        assert!(table.is_empty());
    }

    #[test]
    fn a_late_invalid_value_leaves_every_column_unchanged() {
        let mut table = Table::new(full_schema());
        table
            .insert_rows(vec![vec![
                7.into(),
                4.5.into(),
                true.into(),
                "existing".into(),
            ]])
            .unwrap();
        let before = table.clone();

        let error = table.insert_rows(vec![
            vec![8.into(), 8.5.into(), false.into(), "valid".into()],
            vec![9.into(), f64::NAN.into(), true.into(), "invalid".into()],
        ]);

        assert_eq!(
            error,
            Err(InsertError::NonFiniteFloat {
                row: 1,
                column: 1,
                column_name: "score".to_owned(),
            })
        );
        assert_eq!(table, before);
    }
}
