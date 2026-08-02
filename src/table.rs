//! Typed, column-oriented in-memory table storage.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

/// A data type supported by the in-memory table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// An IEEE 754 double-precision floating-point number.
    Float64,
    /// A Boolean value.
    Bool,
    /// An owned UTF-8 String.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
}

impl ColumnSchema {
    /// Creates a column definition with the supplied name and type.
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    /// Returns the column name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the column's data type.
    pub fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// An ordered table schema with efficient name-based lookup.
#[derive(Debug, Clone)]
pub struct Schema {
    columns: Vec<ColumnSchema>,
    indexes: HashMap<String, usize>,
}

impl Schema {
    /// Builds a schema, rejecting duplicate column names.
    ///
    /// # Errors
    ///
    /// Returns [`TableError::DuplicateColumnName`] when two columns have
    /// exactly the same name.
    pub fn new(columns: Vec<ColumnSchema>) -> Result<Self, TableError> {
        let mut indexes = HashMap::with_capacity(columns.len());

        for (index, column) in columns.iter().enumerate() {
            if indexes.insert(column.name.clone(), index).is_some() {
                return Err(TableError::DuplicateColumnName {
                    name: column.name.clone(),
                });
            }
        }

        Ok(Self { columns, indexes })
    }

    /// Returns all column definitions in schema order.
    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    /// Returns the number of columns in the schema.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns whether the schema contains no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Returns the definition of the column named `name`, if present.
    pub fn column(&self, name: &str) -> Option<&ColumnSchema> {
        self.index_of(name).map(|index| &self.columns[index])
    }

    /// Returns the position of the column named `name`, if present.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.indexes.get(name).copied()
    }
}

impl PartialEq for Schema {
    fn eq(&self, other: &Self) -> bool {
        self.columns == other.columns
    }
}

impl Eq for Schema {}

/// An owned value accepted by row and batch insertion.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer value.
    Int64(
        /// Stored integer.
        i64,
    ),
    /// An IEEE 754 double-precision floating-point value.
    Float64(
        /// Stored floating-point number.
        f64,
    ),
    /// A Boolean value.
    Bool(
        /// Stored Boolean.
        bool,
    ),
    /// An owned UTF-8 String value.
    String(
        /// Stored String.
        String,
    ),
}

impl Value {
    /// Returns the RustHouse data type of this value.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }
}

/// Physical storage for one typed column.
///
/// Each variant owns a contiguous vector of the corresponding Rust type; rows
/// are not retained after insertion.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// Contiguous signed 64-bit integer storage.
    Int64(
        /// Values in row order.
        Vec<i64>,
    ),
    /// Contiguous double-precision floating-point storage.
    Float64(
        /// Values in row order.
        Vec<f64>,
    ),
    /// Contiguous Boolean storage.
    Bool(
        /// Values in row order.
        Vec<bool>,
    ),
    /// Contiguous owned UTF-8 String storage.
    String(
        /// Values in row order.
        Vec<String>,
    ),
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

    /// Returns the data type stored by this column.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of values in the column.
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    /// Returns whether the column contains no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the integer values, or `None` for a differently typed column.
    pub fn as_int64(&self) -> Option<&[i64]> {
        match self {
            Self::Int64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the float values, or `None` for a differently typed column.
    pub fn as_float64(&self) -> Option<&[f64]> {
        match self {
            Self::Float64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the Boolean values, or `None` for a differently typed column.
    pub fn as_bool(&self) -> Option<&[bool]> {
        match self {
            Self::Bool(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the String values, or `None` for a differently typed column.
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
            _ => unreachable!("values are type-checked before columns are mutated"),
        }
    }
}

/// Resource bounds enforced by a [`Table`].
///
/// `max_string_bytes` limits the total UTF-8 payload stored across all String
/// columns, rather than the size of an individual value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableLimits {
    /// Maximum number of columns in a table.
    pub max_columns: usize,
    /// Maximum number of rows stored in a table.
    pub max_rows: usize,
    /// Maximum total UTF-8 payload across all String columns, in bytes.
    pub max_string_bytes: usize,
}

impl Default for TableLimits {
    fn default() -> Self {
        Self {
            max_columns: 1_024,
            max_rows: 1_000_000,
            max_string_bytes: 64 * 1024 * 1024,
        }
    }
}

/// A typed, column-oriented in-memory table.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    limits: TableLimits,
    row_count: usize,
    string_bytes: usize,
}

impl Table {
    /// Creates an empty table after checking the configured column limit.
    ///
    /// # Errors
    ///
    /// Returns [`TableError::ColumnLimitExceeded`] when the schema is wider
    /// than `limits.max_columns`.
    pub fn new(schema: Schema, limits: TableLimits) -> Result<Self, TableError> {
        if schema.len() > limits.max_columns {
            return Err(TableError::ColumnLimitExceeded {
                limit: limits.max_columns,
                attempted: schema.len(),
            });
        }

        let columns = schema
            .columns()
            .iter()
            .map(|column| Column::new(column.data_type()))
            .collect();

        Ok(Self {
            schema,
            columns,
            limits,
            row_count: 0,
            string_bytes: 0,
        })
    }

    /// Returns the table schema.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Returns the resource limits enforced by this table.
    pub fn limits(&self) -> TableLimits {
        self.limits
    }

    /// Returns physical columns in schema order.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns the physical column named `name`, if present.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.schema.index_of(name).map(|index| &self.columns[index])
    }

    /// Returns the number of stored rows.
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns whether the table contains no rows.
    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Returns the total UTF-8 payload stored in all String columns, in bytes.
    pub fn string_bytes(&self) -> usize {
        self.string_bytes
    }

    /// Inserts one row using the same atomic validation as batch insertion.
    ///
    /// # Errors
    ///
    /// Returns [`TableError`] if the row exceeds a resource limit or does not
    /// match the schema. The table is unchanged on error.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<(), TableError> {
        self.insert_batch(vec![row])
    }

    /// Atomically inserts a batch of rows.
    ///
    /// Row count, shape, value types, and cumulative String bytes are checked
    /// for the entire batch before any column is changed. A validation error
    /// therefore leaves the table exactly as it was before the call.
    ///
    /// # Errors
    ///
    /// Returns [`TableError`] if the batch exceeds a resource limit or any row
    /// does not match the schema. The table is unchanged on error.
    pub fn insert_batch(&mut self, rows: Vec<Vec<Value>>) -> Result<(), TableError> {
        let attempted_rows =
            self.row_count
                .checked_add(rows.len())
                .ok_or(TableError::RowLimitExceeded {
                    limit: self.limits.max_rows,
                    attempted: usize::MAX,
                })?;
        if attempted_rows > self.limits.max_rows {
            return Err(TableError::RowLimitExceeded {
                limit: self.limits.max_rows,
                attempted: attempted_rows,
            });
        }

        let mut added_string_bytes = 0usize;
        for (row_index, row) in rows.iter().enumerate() {
            if row.len() != self.schema.len() {
                return Err(TableError::RowShapeMismatch {
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
                    return Err(TableError::TypeMismatch {
                        row: row_index,
                        column: column_index,
                        column_name: column.name().to_owned(),
                        expected,
                        actual,
                    });
                }

                if let Value::String(value) = value {
                    added_string_bytes = added_string_bytes.checked_add(value.len()).ok_or(
                        TableError::StringByteLimitExceeded {
                            limit: self.limits.max_string_bytes,
                            attempted: usize::MAX,
                        },
                    )?;
                }
            }
        }

        let attempted_string_bytes = self.string_bytes.checked_add(added_string_bytes).ok_or(
            TableError::StringByteLimitExceeded {
                limit: self.limits.max_string_bytes,
                attempted: usize::MAX,
            },
        )?;
        if attempted_string_bytes > self.limits.max_string_bytes {
            return Err(TableError::StringByteLimitExceeded {
                limit: self.limits.max_string_bytes,
                attempted: attempted_string_bytes,
            });
        }

        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count = attempted_rows;
        self.string_bytes = attempted_string_bytes;

        Ok(())
    }
}

/// A schema, resource-bound, or row-validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    /// A schema contains the same column name more than once.
    DuplicateColumnName {
        /// Repeated column name.
        name: String,
    },
    /// A schema is wider than the table's configured column limit.
    ColumnLimitExceeded {
        /// Configured maximum number of columns.
        limit: usize,
        /// Number of columns in the supplied schema.
        attempted: usize,
    },
    /// An insertion would exceed the table's configured row limit.
    RowLimitExceeded {
        /// Configured maximum number of rows.
        limit: usize,
        /// Total rows that the insertion attempted to store.
        attempted: usize,
    },
    /// An insertion would exceed the table's configured String byte limit.
    StringByteLimitExceeded {
        /// Configured maximum UTF-8 String payload, in bytes.
        limit: usize,
        /// Total UTF-8 String payload that the insertion attempted, in bytes.
        attempted: usize,
    },
    /// A row has a different number of values than the schema has columns.
    RowShapeMismatch {
        /// Zero-based position of the invalid row within the supplied batch.
        row: usize,
        /// Number of values required by the schema.
        expected: usize,
        /// Number of values supplied in the row.
        actual: usize,
    },
    /// A value's type does not match its schema column.
    TypeMismatch {
        /// Zero-based position of the invalid row within the supplied batch.
        row: usize,
        /// Zero-based position of the invalid value within its row.
        column: usize,
        /// Name of the schema column containing the invalid value.
        column_name: String,
        /// Data type required by the schema.
        expected: DataType,
        /// Data type of the supplied value.
        actual: DataType,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateColumnName { name } => {
                write!(formatter, "duplicate column name: {name}")
            }
            Self::ColumnLimitExceeded { limit, attempted } => write!(
                formatter,
                "column limit exceeded: limit is {limit}, attempted {attempted}"
            ),
            Self::RowLimitExceeded { limit, attempted } => write!(
                formatter,
                "row limit exceeded: limit is {limit}, attempted {attempted}"
            ),
            Self::StringByteLimitExceeded { limit, attempted } => write!(
                formatter,
                "String byte limit exceeded: limit is {limit}, attempted {attempted}"
            ),
            Self::RowShapeMismatch {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "row {row} has {actual} values, expected {expected}"
            ),
            Self::TypeMismatch {
                row,
                column,
                column_name,
                expected,
                actual,
            } => write!(
                formatter,
                "row {row}, column {column} ({column_name}) has type {actual}, expected {expected}"
            ),
        }
    }
}

impl Error for TableError {}
