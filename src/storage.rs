use std::error::Error;
use std::fmt;

/// A logical type supported by RustHouse storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
}

/// The metadata for a column in a [`Schema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
    nullable: bool,
}

impl ColumnSchema {
    /// Returns the column name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the column's logical type.
    pub fn data_type(&self) -> DataType {
        self.data_type
    }

    /// Returns whether the column accepts `NULL` values.
    pub fn is_nullable(&self) -> bool {
        self.nullable
    }
}

/// The schema for a table containing one `Int64` column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    column: ColumnSchema,
}

impl Schema {
    /// Creates a schema with one named `Int64` column.
    pub fn int64(name: impl Into<String>, nullable: bool) -> Self {
        Self {
            column: ColumnSchema {
                name: name.into(),
                data_type: DataType::Int64,
                nullable,
            },
        }
    }

    /// Returns the schema's only column.
    pub fn column(&self) -> &ColumnSchema {
        &self.column
    }

    /// Returns all columns in the schema.
    pub fn columns(&self) -> &[ColumnSchema] {
        std::slice::from_ref(&self.column)
    }
}

/// An error produced when inserting rows into an [`Int64Table`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertError {
    /// A `NULL` value was supplied to a non-nullable column.
    NullNotAllowed { column: String },
    /// Appending rows would exceed the table's configured row cap.
    RowCapExceeded {
        row_cap: usize,
        current_rows: usize,
        incoming_rows: usize,
    },
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullNotAllowed { column } => {
                write!(formatter, "column '{column}' does not allow NULL values")
            }
            Self::RowCapExceeded {
                row_cap,
                current_rows,
                incoming_rows,
            } => write!(
                formatter,
                "appending {incoming_rows} rows to {current_rows} rows exceeds the row cap of {row_cap}"
            ),
        }
    }
}

impl Error for InsertError {}

/// A bounded, one-column table backed by contiguous nullable `Int64` values.
///
/// The row cap bounds the number of stored values. Failed appends leave the
/// table unchanged.
///
/// # Examples
///
/// ```
/// use rusthouse::{Int64Table, Schema};
///
/// let schema = Schema::int64("reading", true);
/// let mut table = Int64Table::new(schema, 3);
/// table.append(Some(7))?;
/// table.append(None)?;
///
/// assert_eq!(table.values(), &[Some(7), None]);
/// # Ok::<(), rusthouse::InsertError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Int64Table {
    schema: Schema,
    values: Vec<Option<i64>>,
    row_cap: usize,
}

impl Int64Table {
    /// Creates an empty table with the supplied schema and maximum row count.
    pub fn new(schema: Schema, row_cap: usize) -> Self {
        Self {
            schema,
            values: Vec::new(),
            row_cap,
        }
    }

    /// Builds the legacy snapshot representation from a validated, non-nullable
    /// physical column without retaining an intermediate row vector.
    #[cfg(unix)]
    pub(crate) fn from_non_nullable_values(
        name: impl Into<String>,
        row_cap: usize,
        values: &[i64],
    ) -> Self {
        debug_assert!(values.len() <= row_cap);
        Self {
            schema: Schema::int64(name, false),
            values: values.iter().copied().map(Some).collect(),
            row_cap,
        }
    }

    /// Returns the table schema.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Returns the configured maximum row count.
    pub fn row_cap(&self) -> usize {
        self.row_cap
    }

    /// Returns the current number of rows.
    pub fn row_count(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the table contains no rows.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns the column values in row order.
    pub fn values(&self) -> &[Option<i64>] {
        &self.values
    }

    /// Consumes the table and returns its column storage.
    pub fn into_values(self) -> Vec<Option<i64>> {
        self.values
    }

    /// Appends one value after validating nullability and the row cap.
    pub fn append(&mut self, value: Option<i64>) -> Result<(), InsertError> {
        self.validate_append(std::slice::from_ref(&value))?;
        self.values.push(value);
        Ok(())
    }

    /// Atomically appends a batch of values.
    pub fn append_batch(&mut self, values: &[Option<i64>]) -> Result<(), InsertError> {
        self.validate_append(values)?;
        self.values.extend_from_slice(values);
        Ok(())
    }

    fn validate_append(&self, values: &[Option<i64>]) -> Result<(), InsertError> {
        if values.len() > self.row_cap.saturating_sub(self.values.len()) {
            return Err(InsertError::RowCapExceeded {
                row_cap: self.row_cap,
                current_rows: self.values.len(),
                incoming_rows: values.len(),
            });
        }

        if !self.schema.column.is_nullable() && values.iter().any(Option::is_none) {
            return Err(InsertError::NullNotAllowed {
                column: self.schema.column.name().to_owned(),
            });
        }

        Ok(())
    }
}
