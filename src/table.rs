//! Typed, columnar, in-memory tables.
//!
//! A [`Table`] owns a fixed schema and stores each physical type in its own
//! contiguous vector. Appending is transactional at the batch level: all rows
//! are validated and transposed before any table data is changed.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// The physical types supported by an in-memory table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// An IEEE 754 double-precision floating-point number.
    Float64,
    /// A boolean value.
    Bool,
    /// A UTF-8 string.
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        })
    }
}

/// A scalar value accepted by [`Table::append_batch`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer.
    Int64(i64),
    /// A double-precision floating-point number.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// A UTF-8 string.
    String(String),
}

impl Value {
    /// Returns the physical type of this value.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
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

/// The name and physical type of one table column.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnDef {
    /// The unique column name within its table.
    pub name: String,
    /// The values accepted by this column.
    pub data_type: DataType,
}

impl ColumnDef {
    /// Creates a column definition.
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    /// Returns the column name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the physical column type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// Contiguous storage for one typed column.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// Signed 64-bit integer values.
    Int64(Vec<i64>),
    /// Double-precision floating-point values.
    Float64(Vec<f64>),
    /// Boolean values.
    Bool(Vec<bool>),
    /// UTF-8 string values.
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

    /// Returns this column's physical type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of stored values.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    /// Returns whether the column contains no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the values when this is an `Int64` column.
    #[must_use]
    pub fn as_int64(&self) -> Option<&[i64]> {
        match self {
            Self::Int64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the values when this is a `Float64` column.
    #[must_use]
    pub fn as_float64(&self) -> Option<&[f64]> {
        match self {
            Self::Float64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the values when this is a `Bool` column.
    #[must_use]
    pub fn as_bool(&self) -> Option<&[bool]> {
        match self {
            Self::Bool(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the values when this is a `String` column.
    #[must_use]
    pub fn as_string(&self) -> Option<&[String]> {
        match self {
            Self::String(values) => Some(values),
            _ => None,
        }
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("values are type-checked before they are staged"),
        }
    }

    fn append(&mut self, other: Self) {
        match (self, other) {
            (Self::Int64(target), Self::Int64(mut source)) => target.append(&mut source),
            (Self::Float64(target), Self::Float64(mut source)) => target.append(&mut source),
            (Self::Bool(target), Self::Bool(mut source)) => target.append(&mut source),
            (Self::String(target), Self::String(mut source)) => target.append(&mut source),
            _ => unreachable!("staged columns always match the table schema"),
        }
    }
}

/// A validation error returned while creating or appending to a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    /// A table must have at least one column.
    EmptySchema,
    /// Two schema entries have the same name.
    DuplicateColumnName {
        /// The repeated column name.
        name: String,
    },
    /// A row does not have one value per schema entry.
    WrongRowWidth {
        /// The zero-based row offset within the submitted batch.
        row: usize,
        /// The schema width.
        expected: usize,
        /// The submitted row width.
        actual: usize,
    },
    /// A value's type differs from its column definition.
    TypeMismatch {
        /// The zero-based row offset within the submitted batch.
        row: usize,
        /// The zero-based column offset.
        column: usize,
        /// The type declared by the schema.
        expected: DataType,
        /// The submitted value's type.
        actual: DataType,
    },
    /// A `Float64` value is NaN or infinite.
    NonFiniteFloat {
        /// The zero-based row offset within the submitted batch.
        row: usize,
        /// The zero-based column offset.
        column: usize,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchema => formatter.write_str("a table schema cannot be empty"),
            Self::DuplicateColumnName { name } => {
                write!(formatter, "duplicate column name `{name}`")
            }
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
                expected,
                actual,
            } => write!(
                formatter,
                "value at row {row}, column {column} has type {actual}; expected {expected}"
            ),
            Self::NonFiniteFloat { row, column } => write!(
                formatter,
                "value at row {row}, column {column} is not a finite Float64"
            ),
        }
    }
}

impl Error for TableError {}

/// A fixed-schema, typed columnar table.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    /// Creates an empty table with the supplied schema.
    ///
    /// Schemas with no columns or repeated column names are rejected.
    pub fn new(schema: Vec<ColumnDef>) -> Result<Self, TableError> {
        if schema.is_empty() {
            return Err(TableError::EmptySchema);
        }

        let mut names = HashSet::with_capacity(schema.len());
        for definition in &schema {
            if !names.insert(definition.name.as_str()) {
                return Err(TableError::DuplicateColumnName {
                    name: definition.name.clone(),
                });
            }
        }

        let columns = schema
            .iter()
            .map(|definition| Column::empty(definition.data_type))
            .collect();

        Ok(Self {
            schema,
            columns,
            row_count: 0,
        })
    }

    /// Returns the fixed table schema in column order.
    #[must_use]
    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    /// Returns all physical columns in schema order.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns a physical column by name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.schema
            .iter()
            .position(|definition| definition.name == name)
            .map(|index| &self.columns[index])
    }

    /// Returns the number of rows in the table.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the number of rows in the table.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.row_count
    }

    /// Returns whether the table contains no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Validates and appends a batch of rows atomically.
    ///
    /// Values are cloned into temporary physical columns while they are
    /// validated. If any row is invalid, no table values or row counts are
    /// changed.
    pub fn append_batch<I, R>(&mut self, rows: I) -> Result<(), TableError>
    where
        I: IntoIterator<Item = R>,
        R: AsRef<[Value]>,
    {
        let mut staged: Vec<Column> = self
            .schema
            .iter()
            .map(|definition| Column::empty(definition.data_type))
            .collect();

        for (row_index, row) in rows.into_iter().enumerate() {
            let row = row.as_ref();
            if row.len() != self.schema.len() {
                return Err(TableError::WrongRowWidth {
                    row: row_index,
                    expected: self.schema.len(),
                    actual: row.len(),
                });
            }

            for (column_index, value) in row.iter().cloned().enumerate() {
                let expected = self.schema[column_index].data_type;
                let actual = value.data_type();
                if actual != expected {
                    return Err(TableError::TypeMismatch {
                        row: row_index,
                        column: column_index,
                        expected,
                        actual,
                    });
                }
                if let Value::Float64(value) = &value
                    && !value.is_finite()
                {
                    return Err(TableError::NonFiniteFloat {
                        row: row_index,
                        column: column_index,
                    });
                }
                staged[column_index].push(value);
            }
        }

        let appended = staged[0].len();
        for (column, staged_column) in self.columns.iter_mut().zip(staged) {
            column.append(staged_column);
        }
        self.row_count += appended;
        Ok(())
    }

    /// Alias for [`Table::append_batch`].
    pub fn append_rows<I, R>(&mut self, rows: I) -> Result<(), TableError>
    where
        I: IntoIterator<Item = R>,
        R: AsRef<[Value]>,
    {
        self.append_batch(rows)
    }

    /// Validates and appends one row.
    pub fn append_row<R>(&mut self, row: R) -> Result<(), TableError>
    where
        R: AsRef<[Value]>,
    {
        self.append_batch(std::iter::once(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Vec<ColumnDef> {
        vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("score", DataType::Float64),
            ColumnDef::new("active", DataType::Bool),
            ColumnDef::new("name", DataType::String),
        ]
    }

    #[test]
    fn rejects_empty_and_duplicate_schemas() {
        assert_eq!(Table::new(vec![]), Err(TableError::EmptySchema));
        assert_eq!(
            Table::new(vec![
                ColumnDef::new("id", DataType::Int64),
                ColumnDef::new("id", DataType::String),
            ]),
            Err(TableError::DuplicateColumnName {
                name: "id".to_owned(),
            })
        );
    }

    #[test]
    fn appends_rows_into_separate_typed_columns() {
        let mut table = Table::new(schema()).unwrap();

        table
            .append_batch(vec![
                vec![1_i64.into(), 1.5.into(), true.into(), "one".into()],
                vec![2_i64.into(), 2.5.into(), false.into(), "two".into()],
            ])
            .unwrap();

        assert_eq!(table.row_count(), 2);
        assert_eq!(table.column("id").unwrap().as_int64(), Some(&[1, 2][..]));
        assert_eq!(
            table.column("score").unwrap().as_float64(),
            Some(&[1.5, 2.5][..])
        );
        assert_eq!(
            table.column("active").unwrap().as_bool(),
            Some(&[true, false][..])
        );
        assert_eq!(
            table.column("name").unwrap().as_string(),
            Some(&["one".to_owned(), "two".to_owned()][..])
        );
    }

    #[test]
    fn invalid_row_rolls_back_the_entire_batch() {
        let mut table = Table::new(schema()).unwrap();
        table
            .append_row(vec![
                1_i64.into(),
                1.0.into(),
                true.into(),
                "existing".into(),
            ])
            .unwrap();
        let before = table.clone();

        let invalid_batch = vec![
            vec![2_i64.into(), 2.0.into(), false.into(), "valid".into()],
            vec![
                3_i64.into(),
                f64::INFINITY.into(),
                true.into(),
                "bad".into(),
            ],
        ];
        let error = table.append_batch(&invalid_batch).unwrap_err();

        assert_eq!(error, TableError::NonFiniteFloat { row: 1, column: 1 });
        assert_eq!(table, before);
    }

    #[test]
    fn rejects_each_invalid_row_shape_without_mutating() {
        let mut table = Table::new(schema()).unwrap();
        let empty = table.clone();

        assert_eq!(
            table.append_batch(vec![vec![1_i64.into()]]),
            Err(TableError::WrongRowWidth {
                row: 0,
                expected: 4,
                actual: 1,
            })
        );
        assert_eq!(table, empty);

        assert_eq!(
            table.append_batch(vec![vec![
                Value::Bool(true),
                1.0.into(),
                true.into(),
                "wrong id".into(),
            ]]),
            Err(TableError::TypeMismatch {
                row: 0,
                column: 0,
                expected: DataType::Int64,
                actual: DataType::Bool,
            })
        );
        assert_eq!(table, empty);

        assert_eq!(
            table.append_batch(vec![vec![
                1_i64.into(),
                f64::NAN.into(),
                true.into(),
                "nan".into(),
            ]]),
            Err(TableError::NonFiniteFloat { row: 0, column: 1 })
        );
        assert_eq!(table, empty);
    }
}
