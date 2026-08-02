//! Typed, column-oriented in-memory table storage.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

/// A data type supported by the in-memory storage layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// A 64-bit IEEE 754 floating-point number.
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

/// A named, typed field in a [`Schema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    name: String,
    data_type: DataType,
}

impl Field {
    /// Creates a field.
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    /// Returns the field name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the field data type.
    pub fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// An error found while validating a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// A table must contain at least one field.
    EmptySchema,
    /// A field name is empty.
    EmptyFieldName {
        /// The zero-based position of the invalid field.
        index: usize,
    },
    /// More than one field has the same name.
    DuplicateFieldName {
        /// The duplicated name.
        name: String,
        /// The zero-based position where the name first appeared.
        first_index: usize,
        /// The zero-based position of the duplicate.
        duplicate_index: usize,
    },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchema => {
                formatter.write_str("a table schema must contain at least one field")
            }
            Self::EmptyFieldName { index } => {
                write!(formatter, "field at index {index} has an empty name")
            }
            Self::DuplicateFieldName {
                name,
                first_index,
                duplicate_index,
            } => write!(
                formatter,
                "field name '{name}' at index {duplicate_index} duplicates index {first_index}"
            ),
        }
    }
}

impl Error for SchemaError {}

/// A validated table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    /// Builds a schema, rejecting missing fields, empty names, and duplicate names.
    pub fn new(fields: Vec<Field>) -> Result<Self, SchemaError> {
        if fields.is_empty() {
            return Err(SchemaError::EmptySchema);
        }

        let mut field_indexes = HashMap::with_capacity(fields.len());
        for (index, field) in fields.iter().enumerate() {
            if field.name.is_empty() {
                return Err(SchemaError::EmptyFieldName { index });
            }
            if let Some(&first_index) = field_indexes.get(field.name.as_str()) {
                return Err(SchemaError::DuplicateFieldName {
                    name: field.name.clone(),
                    first_index,
                    duplicate_index: index,
                });
            }
            field_indexes.insert(field.name.as_str(), index);
        }

        Ok(Self { fields })
    }

    /// Returns the fields in declaration order.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Returns the number of fields.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Returns whether the schema has no fields.
    ///
    /// A successfully constructed schema is never empty.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Finds a field's zero-based position by its case-sensitive name.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|field| field.name == name)
    }
}

/// A scalar value accepted at the row insertion boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer value.
    Int64(i64),
    /// A 64-bit IEEE 754 floating-point value.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// A UTF-8 string value.
    String(String),
}

impl Value {
    /// Returns this value's storage type.
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

/// A homogeneous, contiguous column of values.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// A column backed by `Vec<i64>`.
    Int64(Vec<i64>),
    /// A column backed by `Vec<f64>`.
    Float64(Vec<f64>),
    /// A column backed by `Vec<bool>`.
    Bool(Vec<bool>),
    /// A column backed by `Vec<String>`.
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

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(column), Value::Int64(value)) => column.push(value),
            (Self::Float64(column), Value::Float64(value)) => column.push(value),
            (Self::Bool(column), Value::Bool(value)) => column.push(value),
            (Self::String(column), Value::String(value)) => column.push(value),
            _ => unreachable!("row types are validated before column mutation"),
        }
    }

    /// Returns the type of values stored in this column.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of values stored in this column.
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(column) => column.len(),
            Self::Float64(column) => column.len(),
            Self::Bool(column) => column.len(),
            Self::String(column) => column.len(),
        }
    }

    /// Returns whether the column contains no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the values when this is an `Int64` column.
    pub fn as_int64(&self) -> Option<&[i64]> {
        match self {
            Self::Int64(column) => Some(column),
            _ => None,
        }
    }

    /// Returns the values when this is a `Float64` column.
    pub fn as_float64(&self) -> Option<&[f64]> {
        match self {
            Self::Float64(column) => Some(column),
            _ => None,
        }
    }

    /// Returns the values when this is a `Bool` column.
    pub fn as_bool(&self) -> Option<&[bool]> {
        match self {
            Self::Bool(column) => Some(column),
            _ => None,
        }
    }

    /// Returns the values when this is a `String` column.
    pub fn as_string(&self) -> Option<&[String]> {
        match self {
            Self::String(column) => Some(column),
            _ => None,
        }
    }
}

/// An error returned when a row does not match a table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertError {
    /// The row contains a different number of values than the schema.
    ArityMismatch {
        /// The number of fields in the schema.
        expected: usize,
        /// The number of values supplied by the caller.
        actual: usize,
    },
    /// A value's type does not match its field.
    TypeMismatch {
        /// The zero-based position of the invalid value.
        column: usize,
        /// The name of the field at `column`.
        field: String,
        /// The type declared by the schema.
        expected: DataType,
        /// The supplied value's type.
        actual: DataType,
    },
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArityMismatch { expected, actual } => write!(
                formatter,
                "row has {actual} values but the schema requires {expected}"
            ),
            Self::TypeMismatch {
                column,
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "value for field '{field}' at column {column} has type {actual}, expected {expected}"
            ),
        }
    }
}

impl Error for InsertError {}

/// An in-memory table that stores each field in a homogeneous vector.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    /// Creates an empty table after validating its fields.
    pub fn new(fields: Vec<Field>) -> Result<Self, SchemaError> {
        Ok(Self::from_schema(Schema::new(fields)?))
    }

    /// Creates an empty table from an already validated schema.
    pub fn from_schema(schema: Schema) -> Self {
        let columns = schema
            .fields
            .iter()
            .map(|field| Column::empty(field.data_type))
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

    /// Returns all columns in schema order.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns a column by its zero-based position.
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    /// Returns a column by its case-sensitive field name.
    pub fn column_by_name(&self, name: &str) -> Option<&Column> {
        self.schema
            .index_of(name)
            .and_then(|index| self.columns.get(index))
    }

    /// Returns the number of inserted rows.
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Inserts one row after validating its arity and all value types.
    ///
    /// Validation completes before any column is changed, so an error leaves
    /// the table exactly as it was before the call.
    pub fn insert(&mut self, row: Vec<Value>) -> Result<(), InsertError> {
        if row.len() != self.schema.len() {
            return Err(InsertError::ArityMismatch {
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (column, (field, value)) in self.schema.fields.iter().zip(&row).enumerate() {
            let actual = value.data_type();
            if actual != field.data_type {
                return Err(InsertError::TypeMismatch {
                    column,
                    field: field.name.clone(),
                    expected: field.data_type,
                    actual,
                });
            }
        }

        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
        Ok(())
    }
}
