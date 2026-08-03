use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// A physical type supported by RustHouse's columnar storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// A 64-bit floating-point number.
    Float64,
    /// A boolean value.
    Bool,
    /// A UTF-8 string.
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

/// The name and physical type of one column in a [`Schema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
}

impl ColumnSchema {
    /// Creates a column definition.
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

    /// Returns the column's physical type.
    pub fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// A validated, ordered collection of column definitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    columns: Vec<ColumnSchema>,
}

impl Schema {
    /// Creates a schema, rejecting duplicate column names.
    pub fn new(columns: Vec<ColumnSchema>) -> Result<Self, SchemaError> {
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

    /// Returns the ordered column definitions.
    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    /// Returns the number of columns.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns whether the schema contains no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Returns the definition for a named column.
    pub fn column(&self, name: &str) -> Option<&ColumnSchema> {
        self.column_index(name).map(|index| &self.columns[index])
    }

    /// Returns the position of a named column.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }
}

/// An error encountered while defining a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// Two columns use the same name.
    DuplicateColumn { name: String },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateColumn { name } => write!(formatter, "duplicate column name: {name}"),
        }
    }
}

impl Error for SchemaError {}

/// An owned cell value accepted by [`Table::insert_row`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer.
    Int64(i64),
    /// A 64-bit floating-point number.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// A UTF-8 string.
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

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int64(value) => value.fmt(formatter),
            Self::Float64(value) => value.fmt(formatter),
            Self::Bool(value) => value.fmt(formatter),
            Self::String(value) => formatter.write_str(value),
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

/// A borrowed cell value returned by column and table accessors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueRef<'a> {
    /// A signed 64-bit integer.
    Int64(i64),
    /// A 64-bit floating-point number.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// A borrowed UTF-8 string.
    String(&'a str),
}

impl ValueRef<'_> {
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

/// A type-specific column buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// Signed 64-bit integer values.
    Int64(Vec<i64>),
    /// 64-bit floating-point values.
    Float64(Vec<f64>),
    /// Boolean values.
    Bool(Vec<bool>),
    /// UTF-8 string values.
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

    /// Returns the column's physical type.
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

    /// Returns the value at `row_index`.
    pub fn get(&self, row_index: usize) -> Option<ValueRef<'_>> {
        match self {
            Self::Int64(values) => values.get(row_index).copied().map(ValueRef::Int64),
            Self::Float64(values) => values.get(row_index).copied().map(ValueRef::Float64),
            Self::Bool(values) => values.get(row_index).copied().map(ValueRef::Bool),
            Self::String(values) => values
                .get(row_index)
                .map(String::as_str)
                .map(ValueRef::String),
        }
    }

    /// Returns this column's values when it is an `Int64` column.
    pub fn as_int64(&self) -> Option<&[i64]> {
        match self {
            Self::Int64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns this column's values when it is a `Float64` column.
    pub fn as_float64(&self) -> Option<&[f64]> {
        match self {
            Self::Float64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns this column's values when it is a `Bool` column.
    pub fn as_bool(&self) -> Option<&[bool]> {
        match self {
            Self::Bool(values) => Some(values),
            _ => None,
        }
    }

    /// Returns this column's values when it is a `String` column.
    pub fn as_string(&self) -> Option<&[String]> {
        match self {
            Self::String(values) => Some(values),
            _ => None,
        }
    }
}

/// A bounded, in-memory table backed by one typed vector per column.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
    row_limit: usize,
}

impl Table {
    /// Creates an empty table that can contain at most `row_limit` rows.
    ///
    /// The limit does not allocate memory up front.
    pub fn new(schema: Schema, row_limit: usize) -> Self {
        let columns = schema
            .columns()
            .iter()
            .map(|column| Column::new(column.data_type()))
            .collect();

        Self {
            schema,
            columns,
            row_count: 0,
            row_limit,
        }
    }

    /// Returns this table's schema.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Returns the typed column buffers in schema order.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns a typed column buffer by name.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.schema
            .column_index(name)
            .map(|index| &self.columns[index])
    }

    /// Returns the value at a row and column position.
    pub fn value(&self, row_index: usize, column_index: usize) -> Option<ValueRef<'_>> {
        self.columns
            .get(column_index)
            .and_then(|column| column.get(row_index))
    }

    /// Returns the number of inserted rows.
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the configured maximum number of rows.
    pub fn row_limit(&self) -> usize {
        self.row_limit
    }

    /// Inserts a row after validating the entire row without mutation.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<(), InsertError> {
        if row.len() != self.schema.len() {
            return Err(InsertError::ArityMismatch {
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (column_index, (definition, value)) in
            self.schema.columns().iter().zip(&row).enumerate()
        {
            let actual = value.data_type();
            let expected = definition.data_type();
            if actual != expected {
                return Err(InsertError::TypeMismatch {
                    column_index,
                    column_name: definition.name().to_owned(),
                    expected,
                    actual,
                });
            }
        }

        if self.row_count >= self.row_limit {
            return Err(InsertError::RowLimitExceeded {
                limit: self.row_limit,
            });
        }

        for (column, value) in self.columns.iter_mut().zip(row) {
            match (column, value) {
                (Column::Int64(values), Value::Int64(value)) => values.push(value),
                (Column::Float64(values), Value::Float64(value)) => values.push(value),
                (Column::Bool(values), Value::Bool(value)) => values.push(value),
                (Column::String(values), Value::String(value)) => values.push(value),
                _ => unreachable!("row types were validated before mutation"),
            }
        }
        self.row_count += 1;

        Ok(())
    }
}

/// An error that prevents a row from being inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertError {
    /// The row has a different number of values than the schema.
    ArityMismatch { expected: usize, actual: usize },
    /// A value does not match its column's declared type.
    TypeMismatch {
        column_index: usize,
        column_name: String,
        expected: DataType,
        actual: DataType,
    },
    /// Inserting the row would exceed the configured table limit.
    RowLimitExceeded { limit: usize },
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArityMismatch { expected, actual } => {
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
                "column {column_name:?} at index {column_index} requires {expected}, got {actual}"
            ),
            Self::RowLimitExceeded { limit } => {
                write!(formatter, "table row limit of {limit} has been reached")
            }
        }
    }
}

impl Error for InsertError {}
