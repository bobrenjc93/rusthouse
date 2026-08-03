//! Typed, in-memory columnar storage for one table.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// Maximum number of rows accepted by [`Table::new`].
///
/// Use [`Table::with_row_limit`] when a workload needs a smaller or larger
/// explicit bound.
pub const DEFAULT_ROW_LIMIT: usize = 1_000_000;

/// A physical type supported by the in-memory columnar store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// An IEEE 754 double-precision floating-point number.
    Float64,
    /// A Boolean value.
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

/// A typed cell value accepted by [`Table::insert_batch`].
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer value.
    Int64(i64),
    /// An IEEE 754 double-precision floating-point value.
    Float64(f64),
    /// A Boolean value.
    Bool(bool),
    /// An owned UTF-8 string value.
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

/// A named column in a table schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    name: String,
    data_type: DataType,
}

impl Field {
    /// Creates a field with the given name and physical type.
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    /// Returns the field name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the field's physical type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// A deterministic validation or capacity error from [`Table`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableError {
    /// A table was constructed without any fields.
    EmptySchema,
    /// A schema field has an empty name.
    EmptyFieldName {
        /// Zero-based position of the invalid field.
        index: usize,
    },
    /// A schema contains the same field name more than once.
    DuplicateField {
        /// The duplicated, case-sensitive field name.
        name: String,
    },
    /// Memory could not be reserved while validating a schema.
    SchemaAllocationFailed {
        /// Number of field names whose validation allocation failed.
        field_count: usize,
    },
    /// A batch would exceed the table's configured row limit.
    RowLimitExceeded {
        /// Configured maximum row count.
        limit: usize,
        /// Row count before the rejected insertion.
        current: usize,
    },
    /// A batch row does not contain one value for every field.
    RowWidthMismatch {
        /// Zero-based position of the invalid row within the batch.
        row: usize,
        /// Number of fields in the table schema.
        expected: usize,
        /// Number of values supplied by the row.
        actual: usize,
    },
    /// A value's type does not match its schema field.
    TypeMismatch {
        /// Zero-based position of the invalid row within the batch.
        row: usize,
        /// Zero-based position of the invalid value within the row.
        column: usize,
        /// Name of the schema field at `column`.
        field: String,
        /// Type required by the schema.
        expected: DataType,
        /// Type of the supplied value.
        actual: DataType,
    },
    /// A requested field does not exist in the schema.
    FieldNotFound {
        /// The requested field name.
        name: String,
    },
    /// A typed column accessor does not match the field's type.
    ColumnTypeMismatch {
        /// The requested field name.
        field: String,
        /// Type requested through the accessor.
        expected: DataType,
        /// Type declared in the schema.
        actual: DataType,
    },
    /// Memory could not be reserved for a validated batch.
    AllocationFailed {
        /// Number of rows in the batch whose allocation failed.
        additional_rows: usize,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchema => {
                formatter.write_str("a table schema must contain at least one field")
            }
            Self::EmptyFieldName { index } => {
                write!(formatter, "schema field at index {index} has an empty name")
            }
            Self::DuplicateField { name } => {
                write!(formatter, "schema contains duplicate field `{name}`")
            }
            Self::SchemaAllocationFailed { field_count } => write!(
                formatter,
                "could not reserve storage to validate {field_count} schema fields"
            ),
            Self::RowLimitExceeded { limit, current } => write!(
                formatter,
                "batch exceeds row limit {limit}; table currently contains {current} rows"
            ),
            Self::RowWidthMismatch {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "batch row {row} has {actual} values; expected {expected}"
            ),
            Self::TypeMismatch {
                row,
                column,
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "batch row {row}, column {column} (`{field}`) has type {actual}; expected {expected}"
            ),
            Self::FieldNotFound { name } => write!(formatter, "field `{name}` does not exist"),
            Self::ColumnTypeMismatch {
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "field `{field}` has type {actual}; requested {expected} column"
            ),
            Self::AllocationFailed { additional_rows } => write!(
                formatter,
                "could not reserve storage for {additional_rows} additional rows"
            ),
        }
    }
}

impl Error for TableError {}

#[derive(Debug)]
pub(crate) enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    // Bytes avoid Vec<bool>'s proxy representation while retaining a compact,
    // contiguous Boolean column. Values are always normalized to 0 or 1.
    Bool(Vec<u8>),
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

    fn try_reserve(&mut self, additional: usize) -> Result<(), TableError> {
        let result = match self {
            Self::Int64(values) => values.try_reserve(additional),
            Self::Float64(values) => values.try_reserve(additional),
            Self::Bool(values) => values.try_reserve(additional),
            Self::String(values) => values.try_reserve(additional),
        };
        result.map_err(|_| TableError::AllocationFailed {
            additional_rows: additional,
        })
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(u8::from(value)),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("values are type-checked before columns are mutated"),
        }
    }

    pub(crate) const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }
}

/// A bounded, typed, in-memory columnar table.
///
/// Each schema field owns one contiguous vector of its physical type. String
/// columns store their `String` headers contiguously and own each UTF-8 buffer.
/// Insertion is atomic with respect to validation: the complete batch is
/// checked for capacity, row width, and value types before any column receives
/// a value.
///
/// # Example
///
/// ```
/// use rusthouse::{DataType, Field, Table, Value};
///
/// let mut events = Table::with_row_limit(
///     vec![
///         Field::new("id", DataType::Int64),
///         Field::new("ok", DataType::Bool),
///     ],
///     100,
/// )?;
///
/// events.insert_batch(vec![
///     vec![Value::Int64(1), Value::Bool(true)],
///     vec![Value::Int64(2), Value::Bool(false)],
/// ])?;
///
/// assert_eq!(events.int64_column("id")?, &[1, 2]);
/// assert_eq!(events.bool_column("ok")?.collect::<Vec<_>>(), [true, false]);
/// # Ok::<(), rusthouse::TableError>(())
/// ```
#[derive(Debug)]
pub struct Table {
    fields: Vec<Field>,
    columns: Vec<Column>,
    row_count: usize,
    row_limit: usize,
}

impl Table {
    /// Creates an empty table bounded by [`DEFAULT_ROW_LIMIT`].
    ///
    /// Field names are case-sensitive and must be non-empty and unique.
    pub fn new(fields: Vec<Field>) -> Result<Self, TableError> {
        Self::with_row_limit(fields, DEFAULT_ROW_LIMIT)
    }

    /// Creates an empty table with an explicit maximum row count.
    ///
    /// A limit of zero is valid and creates a schema that rejects every
    /// non-empty batch.
    pub fn with_row_limit(fields: Vec<Field>, row_limit: usize) -> Result<Self, TableError> {
        validate_fields(&fields)?;
        let columns = fields
            .iter()
            .map(|field| Column::new(field.data_type))
            .collect();
        Ok(Self {
            fields,
            columns,
            row_count: 0,
            row_limit,
        })
    }

    /// Returns the table schema in column order.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Returns the number of rows in every column.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.row_count
    }

    /// Returns whether the table contains no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Returns the configured maximum row count.
    #[must_use]
    pub const fn row_limit(&self) -> usize {
        self.row_limit
    }

    pub(crate) fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub(crate) fn from_snapshot_parts(
        fields: Vec<Field>,
        columns: Vec<Column>,
        row_count: usize,
        row_limit: usize,
    ) -> Self {
        debug_assert_eq!(fields.len(), columns.len());
        debug_assert!(row_count <= row_limit);
        debug_assert!(fields.iter().zip(&columns).all(|(field, column)| {
            field.data_type == column.data_type() && column.len() == row_count
        }));
        Self {
            fields,
            columns,
            row_count,
            row_limit,
        }
    }

    /// Validates and appends a batch of owned rows.
    ///
    /// The iterator is consumed only until the configured row bound is
    /// exceeded, so even an unbounded producer cannot make this method buffer
    /// more than the table's remaining capacity plus one row. On any error,
    /// the table's row count and column values remain unchanged.
    ///
    /// Returns the number of inserted rows. An empty batch is a successful
    /// no-op.
    pub fn insert_batch<I>(&mut self, rows: I) -> Result<usize, TableError>
    where
        I: IntoIterator<Item = Vec<Value>>,
    {
        let remaining = self.row_limit - self.row_count;
        let mut batch = Vec::new();
        for row in rows {
            if batch.len() == remaining {
                return Err(TableError::RowLimitExceeded {
                    limit: self.row_limit,
                    current: self.row_count,
                });
            }
            batch
                .try_reserve(1)
                .map_err(|_| TableError::AllocationFailed { additional_rows: 1 })?;
            batch.push(row);
        }

        self.validate_rows(&batch)?;

        let inserted = batch.len();
        for column in &mut self.columns {
            column.try_reserve(inserted)?;
        }
        for row in batch {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count += inserted;
        Ok(inserted)
    }

    /// Returns an `Int64` column as a contiguous slice.
    pub fn int64_column(&self, field: &str) -> Result<&[i64], TableError> {
        match self.column_of_type(field, DataType::Int64)? {
            Column::Int64(values) => Ok(values),
            _ => unreachable!("column type was checked"),
        }
    }

    /// Returns a `Float64` column as a contiguous slice.
    pub fn float64_column(&self, field: &str) -> Result<&[f64], TableError> {
        match self.column_of_type(field, DataType::Float64)? {
            Column::Float64(values) => Ok(values),
            _ => unreachable!("column type was checked"),
        }
    }

    /// Returns a scan iterator over a compact, contiguous `Bool` column.
    ///
    /// Boolean values are stored internally as normalized bytes rather than
    /// Rust's proxy-based `Vec<bool>` representation.
    pub fn bool_column(
        &self,
        field: &str,
    ) -> Result<impl ExactSizeIterator<Item = bool> + DoubleEndedIterator + '_, TableError> {
        match self.column_of_type(field, DataType::Bool)? {
            Column::Bool(values) => Ok(values.iter().map(|value| *value != 0)),
            _ => unreachable!("column type was checked"),
        }
    }

    /// Returns a `String` column as a contiguous slice of owned strings.
    pub fn string_column(&self, field: &str) -> Result<&[String], TableError> {
        match self.column_of_type(field, DataType::String)? {
            Column::String(values) => Ok(values),
            _ => unreachable!("column type was checked"),
        }
    }

    fn validate_rows(&self, rows: &[Vec<Value>]) -> Result<(), TableError> {
        for (row_index, row) in rows.iter().enumerate() {
            if row.len() != self.fields.len() {
                return Err(TableError::RowWidthMismatch {
                    row: row_index,
                    expected: self.fields.len(),
                    actual: row.len(),
                });
            }
            for (column_index, (value, field)) in row.iter().zip(&self.fields).enumerate() {
                let actual = value.data_type();
                if actual != field.data_type {
                    return Err(TableError::TypeMismatch {
                        row: row_index,
                        column: column_index,
                        field: field.name.clone(),
                        expected: field.data_type,
                        actual,
                    });
                }
            }
        }
        Ok(())
    }

    fn column_of_type(&self, name: &str, expected: DataType) -> Result<&Column, TableError> {
        let index = self
            .fields
            .iter()
            .position(|field| field.name == name)
            .ok_or_else(|| TableError::FieldNotFound {
                name: name.to_owned(),
            })?;
        let actual = self.fields[index].data_type;
        if actual != expected {
            return Err(TableError::ColumnTypeMismatch {
                field: name.to_owned(),
                expected,
                actual,
            });
        }
        Ok(&self.columns[index])
    }
}

pub(crate) fn validate_fields(fields: &[Field]) -> Result<(), TableError> {
    if fields.is_empty() {
        return Err(TableError::EmptySchema);
    }

    let mut names = HashSet::new();
    names
        .try_reserve(fields.len())
        .map_err(|_| TableError::SchemaAllocationFailed {
            field_count: fields.len(),
        })?;
    for (index, field) in fields.iter().enumerate() {
        if field.name.is_empty() {
            return Err(TableError::EmptyFieldName { index });
        }
        if !names.insert(field.name.as_str()) {
            let mut name = String::new();
            name.try_reserve_exact(field.name.len()).map_err(|_| {
                TableError::SchemaAllocationFailed {
                    field_count: fields.len(),
                }
            })?;
            name.push_str(&field.name);
            return Err(TableError::DuplicateField { name });
        }
    }
    Ok(())
}
