//! In-memory typed columnar storage.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// A physical and logical column type supported by RustHouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// A finite IEEE 754 double-precision number.
    Float64,
    /// A boolean value.
    Bool,
    /// An owned UTF-8 string.
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

/// A value that can be inserted into a [`Table`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer value.
    Int64(i64),
    /// A double-precision floating-point value.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// An owned UTF-8 string value.
    String(String),
}

impl Value {
    /// Returns this value's exact logical type.
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

/// The name and type of one table column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
}

impl ColumnSchema {
    /// Creates a column definition.
    #[must_use]
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

    /// Returns the column type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// A validated, ordered collection of column definitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    columns: Vec<ColumnSchema>,
}

impl Schema {
    /// Creates a schema, rejecting empty schemas and duplicate column names.
    ///
    /// Column names are compared exactly and are case-sensitive at this
    /// storage boundary.
    pub fn new(columns: Vec<ColumnSchema>) -> Result<Self, SchemaError> {
        if columns.is_empty() {
            return Err(SchemaError::Empty);
        }

        let mut names = HashSet::with_capacity(columns.len());
        for (index, column) in columns.iter().enumerate() {
            if column.name.is_empty() {
                return Err(SchemaError::EmptyColumnName { index });
            }
            if !names.insert(column.name.clone()) {
                return Err(SchemaError::DuplicateColumn {
                    name: column.name.clone(),
                });
            }
        }

        Ok(Self { columns })
    }

    /// Returns the columns in their declared order.
    #[must_use]
    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    /// Returns the number of columns in the schema.
    #[must_use]
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns whether this schema has no columns.
    ///
    /// A constructed `Schema` is never empty; this method is provided for
    /// conventional collection-like inspection.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Returns a column definition by its declaration index.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&ColumnSchema> {
        self.columns.get(index)
    }

    /// Returns a column definition by its exact, case-sensitive name.
    #[must_use]
    pub fn column_by_name(&self, name: &str) -> Option<&ColumnSchema> {
        self.columns.iter().find(|column| column.name == name)
    }
}

/// An error returned while constructing a [`Schema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// A table must have at least one column.
    Empty,
    /// A column name must not be empty.
    EmptyColumnName {
        /// The declaration index of the invalid column.
        index: usize,
    },
    /// Each column name must be unique within a schema.
    DuplicateColumn {
        /// The repeated column name.
        name: String,
    },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a table schema must contain at least one column"),
            Self::EmptyColumnName { index } => {
                write!(formatter, "column at index {index} has an empty name")
            }
            Self::DuplicateColumn { name } => write!(formatter, "duplicate column name `{name}`"),
        }
    }
}

impl Error for SchemaError {}

/// A type-specific physical column.
///
/// The variants deliberately expose the backing `Vec` through immutable
/// access, making the table's columnar layout explicit without allowing
/// callers to break the equal-length column invariant.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// A physically contiguous integer column.
    Int64(Vec<i64>),
    /// A physically contiguous floating-point column.
    Float64(Vec<f64>),
    /// A physically contiguous boolean column.
    Bool(Vec<bool>),
    /// A physically contiguous string column.
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

    /// Returns this column's logical type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of values stored in this column.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    /// Returns whether this column has no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the integer values when this is an `Int64` column.
    #[must_use]
    pub fn as_i64_slice(&self) -> Option<&[i64]> {
        match self {
            Self::Int64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the floating-point values when this is a `Float64` column.
    #[must_use]
    pub fn as_f64_slice(&self) -> Option<&[f64]> {
        match self {
            Self::Float64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the boolean values when this is a `Bool` column.
    #[must_use]
    pub fn as_bool_slice(&self) -> Option<&[bool]> {
        match self {
            Self::Bool(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the string values when this is a `String` column.
    #[must_use]
    pub fn as_string_slice(&self) -> Option<&[String]> {
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
            _ => unreachable!("row values are validated before columns are mutated"),
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
}

/// A table that stores each typed field in its own contiguous vector.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    /// Creates an empty table from a validated schema.
    #[must_use]
    pub fn new(schema: Schema) -> Self {
        let columns = schema
            .columns
            .iter()
            .map(|column| Column::empty(column.data_type))
            .collect();
        Self {
            schema,
            columns,
            row_count: 0,
        }
    }

    /// Returns the table schema.
    #[must_use]
    pub const fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Returns the physical columns in schema order.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns a physical column by declaration index.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    /// Returns the number of successfully inserted rows.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Inserts one row after validating all of its values.
    ///
    /// Validation covers row width, exact logical types, and finite floats.
    /// No physical column is mutated unless the complete row is valid.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<(), InsertError> {
        self.validate_row(&row)?;

        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
        Ok(())
    }

    /// Inserts a batch of rows as one transactional operation.
    ///
    /// Every row is validated before any physical column is changed. If a row
    /// is invalid, the error identifies its zero-based index in the batch and
    /// the table remains unchanged. An empty batch is a no-op.
    pub fn insert_batch(&mut self, rows: Vec<Vec<Value>>) -> Result<(), BatchInsertError> {
        for (batch_index, row) in rows.iter().enumerate() {
            self.validate_row(row).map_err(|source| BatchInsertError {
                batch_index,
                source,
            })?;
        }

        let batch_size = rows.len();
        if batch_size == 0 {
            return Ok(());
        }

        for column in &mut self.columns {
            column.reserve(batch_size);
        }
        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count += batch_size;
        Ok(())
    }

    fn validate_row(&self, row: &[Value]) -> Result<(), InsertError> {
        if row.len() != self.schema.len() {
            return Err(InsertError::RowWidth {
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (index, (column, value)) in self.schema.columns.iter().zip(row).enumerate() {
            let actual = value.data_type();
            if actual != column.data_type {
                return Err(InsertError::TypeMismatch {
                    column_index: index,
                    column_name: column.name.clone(),
                    expected: column.data_type,
                    actual,
                });
            }

            if let Value::Float64(value) = value {
                if !value.is_finite() {
                    return Err(InsertError::NonFiniteFloat {
                        column_index: index,
                        column_name: column.name.clone(),
                        value: *value,
                    });
                }
            }
        }

        Ok(())
    }
}

/// An error returned when a row cannot be inserted.
#[derive(Debug, Clone, PartialEq)]
pub enum InsertError {
    /// The row does not contain exactly one value per schema column.
    RowWidth {
        /// The required number of values.
        expected: usize,
        /// The supplied number of values.
        actual: usize,
    },
    /// A value's type is not exactly the declared column type.
    TypeMismatch {
        /// The declaration index of the affected column.
        column_index: usize,
        /// The name of the affected column.
        column_name: String,
        /// The declared column type.
        expected: DataType,
        /// The supplied value type.
        actual: DataType,
    },
    /// A `Float64` value is NaN or infinite.
    NonFiniteFloat {
        /// The declaration index of the affected column.
        column_index: usize,
        /// The name of the affected column.
        column_name: String,
        /// The rejected floating-point value.
        value: f64,
    },
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowWidth { expected, actual } => {
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
                "column `{column_name}` at index {column_index} requires {expected}, got {actual}"
            ),
            Self::NonFiniteFloat {
                column_index,
                column_name,
                value,
            } => write!(
                formatter,
                "column `{column_name}` at index {column_index} requires a finite Float64, got {value}"
            ),
        }
    }
}

impl Error for InsertError {}

/// An error returned when a row in a batch cannot be inserted.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchInsertError {
    /// The zero-based index of the invalid row within the supplied batch.
    pub batch_index: usize,
    /// The validation error for the invalid row.
    pub source: InsertError,
}

impl fmt::Display for BatchInsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "row at batch index {} is invalid: {}",
            self.batch_index, self.source
        )
    }
}

impl Error for BatchInsertError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}
