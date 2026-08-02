use crate::{DataType, Result, StorageError, Value};

/// The maximum number of rows accepted by [`Table::new`].
///
/// Use [`Table::with_row_limit`] when a workload needs a different bound.
pub const DEFAULT_ROW_LIMIT: usize = 1_000_000;

/// A named, typed field in a table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// The column name. Names are unique within a schema ignoring ASCII case.
    pub name: String,
    /// The physical type stored by the column.
    pub data_type: DataType,
}

impl ColumnDef {
    /// Creates a column definition.
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }
}

/// A physical column backed by a vector containing one Rust type.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// A column of signed 64-bit integers.
    Int64(Vec<i64>),
    /// A column of finite double-precision numbers.
    Float64(Vec<f64>),
    /// A column of booleans.
    Bool(Vec<bool>),
    /// A column of owned UTF-8 strings.
    String(Vec<String>),
}

impl Column {
    /// Creates an empty physical column for `data_type`.
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    /// Returns the column's physical type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of values in the column.
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

    /// Returns a cloned scalar value at `row`, or `None` when out of bounds.
    #[must_use]
    pub fn get(&self, row: usize) -> Option<Value> {
        match self {
            Self::Int64(values) => values.get(row).copied().map(Value::Int64),
            Self::Float64(values) => values.get(row).copied().map(Value::Float64),
            Self::Bool(values) => values.get(row).copied().map(Value::Bool),
            Self::String(values) => values.get(row).cloned().map(Value::String),
        }
    }

    /// Returns a cloned scalar value at `row`.
    ///
    /// # Panics
    ///
    /// Panics when `row` is outside the column. Use [`Self::get`] when the
    /// index is not already known to be below [`Self::len`].
    #[must_use]
    pub fn value(&self, row: usize) -> Value {
        self.get(row).expect("column row index out of bounds")
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("row values are validated before insertion"),
        }
    }
}

/// A typed table storing one physical vector per schema field.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    row_count: usize,
    row_limit: usize,
}

impl Table {
    /// Creates an empty table with [`DEFAULT_ROW_LIMIT`].
    pub fn new(name: impl Into<String>, schema: Vec<ColumnDef>) -> Result<Self> {
        Self::with_row_limit(name, schema, DEFAULT_ROW_LIMIT)
    }

    /// Creates an empty table with an explicit maximum row count.
    ///
    /// A limit of zero is valid and creates a table that rejects every row.
    pub fn with_row_limit(
        name: impl Into<String>,
        schema: Vec<ColumnDef>,
        row_limit: usize,
    ) -> Result<Self> {
        let name = name.into();
        validate_schema(&name, &schema)?;

        let columns = schema
            .iter()
            .map(|field| Column::new(field.data_type))
            .collect();

        Ok(Self {
            name,
            schema,
            columns,
            row_count: 0,
            row_limit,
        })
    }

    /// Returns the table name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the table schema in physical column order.
    #[must_use]
    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    /// Returns the physical columns in schema order.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns the number of stored rows.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the configured maximum row count.
    #[must_use]
    pub const fn row_limit(&self) -> usize {
        self.row_limit
    }

    /// Returns the index of a column, matching its name case-insensitively.
    pub fn column_index(&self, name: &str) -> Result<usize> {
        self.schema
            .iter()
            .position(|field| field.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| StorageError::ColumnNotFound {
                table: self.name.clone(),
                column: name.to_owned(),
            })
    }

    /// Validates a row without mutating the table.
    ///
    /// Validation covers the row limit, row width, value types, and finite
    /// Float64 values. [`Self::insert_row`] performs this complete pass before
    /// appending to any physical column.
    pub fn validate_row(&self, row: &[Value]) -> Result<()> {
        if self.row_count >= self.row_limit {
            return Err(StorageError::RowLimitExceeded {
                table: self.name.clone(),
                limit: self.row_limit,
            });
        }

        if row.len() != self.schema.len() {
            return Err(StorageError::RowLength {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (field, value) in self.schema.iter().zip(row) {
            let actual = value.data_type();
            if field.data_type != actual {
                return Err(StorageError::TypeMismatch {
                    table: self.name.clone(),
                    column: field.name.clone(),
                    expected: field.data_type,
                    actual,
                });
            }

            if matches!(value, Value::Float64(number) if !number.is_finite()) {
                return Err(StorageError::NonFiniteFloat {
                    table: self.name.clone(),
                    column: field.name.clone(),
                });
            }
        }

        Ok(())
    }

    /// Atomically appends one value to every physical column.
    ///
    /// All recoverable validation failures leave every column and the row
    /// count unchanged.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.validate_row(&row)?;

        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;

        Ok(())
    }
}

fn validate_schema(name: &str, schema: &[ColumnDef]) -> Result<()> {
    if name.trim().is_empty() {
        return Err(StorageError::EmptyTableName);
    }
    if schema.is_empty() {
        return Err(StorageError::EmptySchema);
    }

    for (index, field) in schema.iter().enumerate() {
        if field.name.trim().is_empty() {
            return Err(StorageError::EmptyColumnName { index });
        }
        if schema[..index]
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&field.name))
        {
            return Err(StorageError::DuplicateColumn {
                name: field.name.clone(),
            });
        }
    }

    Ok(())
}
