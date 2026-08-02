//! Typed, bounded, in-memory columnar storage.
//!
//! A [`Table`] stores each field in its own type-specific vector. Every column
//! has a bit-packed [`ValidityBitmap`], so a null occupies one placeholder in
//! the values vector while its validity bit is clear.
//!
//! ```
//! use rusthouse::{DataType, Field, Schema, Table, Value};
//!
//! let schema = Schema::new(vec![
//!     Field::new("id", DataType::Int64, false),
//!     Field::new("label", DataType::String, true),
//! ])?;
//! let mut table = Table::new(schema, 100);
//!
//! table.append_row([
//!     ("label", Value::String("first".into())),
//!     ("id", Value::Int64(1)),
//! ])?;
//! assert_eq!(table.row_count(), 1);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;

/// A physical value type supported by the storage layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// An IEEE 754 double-precision number.
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

/// A field in a [`Schema`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Field {
    name: String,
    data_type: DataType,
    nullable: bool,
}

impl Field {
    /// Creates a named field with an explicit type and nullability.
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }

    /// Returns the field name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the physical type of the field.
    pub fn data_type(&self) -> DataType {
        self.data_type
    }

    /// Returns whether the field accepts [`Value::Null`].
    pub fn is_nullable(&self) -> bool {
        self.nullable
    }
}

/// A validated, ordered collection of fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schema {
    fields: Vec<Field>,
    field_indexes: HashMap<String, usize>,
}

impl Schema {
    /// Builds a schema, rejecting duplicate field names.
    pub fn new(fields: Vec<Field>) -> Result<Self, SchemaError> {
        let mut field_indexes = HashMap::with_capacity(fields.len());
        for (index, field) in fields.iter().enumerate() {
            if field_indexes.insert(field.name.clone(), index).is_some() {
                return Err(SchemaError::DuplicateField {
                    field: field.name.clone(),
                });
            }
        }

        Ok(Self {
            fields,
            field_indexes,
        })
    }

    /// Returns the fields in their stable column order.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Returns the field with `name`, if present.
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.index_of(name).map(|index| &self.fields[index])
    }

    /// Returns the number of fields.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Returns whether the schema has no fields.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.field_indexes.get(name).copied()
    }
}

/// An error produced while constructing a [`Schema`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaError {
    /// Two schema fields have the same name.
    DuplicateField {
        /// The repeated field name.
        field: String,
    },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateField { field } => {
                write!(formatter, "schema contains duplicate field `{field}`")
            }
        }
    }
}

impl Error for SchemaError {}

/// A scalar value accepted by [`Table::append_row`].
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// SQL-style NULL, whose concrete type comes from its field.
    Null,
    /// A signed 64-bit integer.
    Int64(i64),
    /// An IEEE 754 double-precision number.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// A UTF-8 string.
    String(String),
}

impl Value {
    fn value_type(&self) -> ValueType {
        match self {
            Self::Null => ValueType::Null,
            Self::Int64(_) => ValueType::Int64,
            Self::Float64(_) => ValueType::Float64,
            Self::Bool(_) => ValueType::Bool,
            Self::String(_) => ValueType::String,
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

/// The runtime kind of a [`Value`], used in type mismatch errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueType {
    /// The untyped null marker.
    Null,
    /// A signed 64-bit integer.
    Int64,
    /// An IEEE 754 double-precision number.
    Float64,
    /// A boolean value.
    Bool,
    /// A UTF-8 string.
    String,
}

impl fmt::Display for ValueType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Null => "NULL",
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        })
    }
}

/// A compact bitmap containing one validity bit per stored value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidityBitmap {
    words: Vec<u64>,
    len: usize,
}

impl ValidityBitmap {
    fn new() -> Self {
        Self {
            words: Vec::new(),
            len: 0,
        }
    }

    /// Returns the validity bit for `index`, or `None` when out of bounds.
    pub fn get(&self, index: usize) -> Option<bool> {
        if index >= self.len {
            return None;
        }

        Some((self.words[index / 64] & (1_u64 << (index % 64))) != 0)
    }

    /// Returns the packed bitmap words, least-significant bit first.
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// Returns the number of represented values.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the bitmap represents no values.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, valid: bool) {
        let bit = self.len % 64;
        if bit == 0 {
            self.words.push(0);
        }
        if valid {
            let last_word = self.words.last_mut().expect("a bitmap word was just added");
            *last_word |= 1_u64 << bit;
        }
        self.len += 1;
    }
}

/// A type-specific values vector and its validity bitmap.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedColumn<T> {
    values: Vec<T>,
    validity: ValidityBitmap,
}

impl<T> TypedColumn<T> {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            validity: ValidityBitmap::new(),
        }
    }

    /// Returns the physical values, including placeholders for nulls.
    pub fn values(&self) -> &[T] {
        &self.values
    }

    /// Returns the column validity bitmap.
    pub fn validity(&self) -> &ValidityBitmap {
        &self.validity
    }

    /// Returns `None` for an out-of-bounds index and otherwise returns the
    /// optional logical value at that index.
    pub fn get(&self, index: usize) -> Option<Option<&T>> {
        let value = self.values.get(index)?;
        Some(self.validity.get(index)?.then_some(value))
    }

    /// Returns the number of logical values, including nulls.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the column has no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn push(&mut self, value: T, valid: bool) {
        self.values.push(value);
        self.validity.push(valid);
    }
}

/// A column backed by a vector matching its schema type.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    /// An `Int64` vector.
    Int64(TypedColumn<i64>),
    /// A `Float64` vector.
    Float64(TypedColumn<f64>),
    /// A `Bool` vector.
    Bool(TypedColumn<bool>),
    /// A `String` vector.
    String(TypedColumn<String>),
}

impl Column {
    fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(TypedColumn::new()),
            DataType::Float64 => Self::Float64(TypedColumn::new()),
            DataType::Bool => Self::Bool(TypedColumn::new()),
            DataType::String => Self::String(TypedColumn::new()),
        }
    }

    /// Returns the physical type of this column.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of logical values, including nulls.
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(column) => column.len(),
            Self::Float64(column) => column.len(),
            Self::Bool(column) => column.len(),
            Self::String(column) => column.len(),
        }
    }

    /// Returns whether the column has no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the column validity bitmap.
    pub fn validity(&self) -> &ValidityBitmap {
        match self {
            Self::Int64(column) => column.validity(),
            Self::Float64(column) => column.validity(),
            Self::Bool(column) => column.validity(),
            Self::String(column) => column.validity(),
        }
    }

    fn push_validated(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(column), Value::Int64(value)) => column.push(value, true),
            (Self::Float64(column), Value::Float64(value)) => column.push(value, true),
            (Self::Bool(column), Value::Bool(value)) => column.push(value, true),
            (Self::String(column), Value::String(value)) => column.push(value, true),
            (Self::Int64(column), Value::Null) => column.push(0, false),
            (Self::Float64(column), Value::Null) => column.push(0.0, false),
            (Self::Bool(column), Value::Null) => column.push(false, false),
            (Self::String(column), Value::Null) => column.push(String::new(), false),
            _ => unreachable!("append values are type-checked before columns are mutated"),
        }
    }
}

/// A bounded table containing one [`Column`] per schema field.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
    row_limit: usize,
}

impl Table {
    /// Creates an empty table that will store at most `row_limit` rows.
    pub fn new(schema: Schema, row_limit: usize) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|field| Column::new(field.data_type()))
            .collect();

        Self {
            schema,
            columns,
            row_count: 0,
            row_limit,
        }
    }

    /// Atomically appends a named row after validating its complete contents.
    ///
    /// Field order does not matter. Duplicate fields, missing or unexpected
    /// fields, type mismatches, disallowed nulls, and rows beyond the configured
    /// limit return an [`AppendError`] without changing the table.
    pub fn append_row<I, N>(&mut self, row: I) -> Result<(), AppendError>
    where
        I: IntoIterator<Item = (N, Value)>,
        N: Into<String>,
    {
        if self.row_count >= self.row_limit {
            return Err(AppendError::RowLimitExceeded {
                limit: self.row_limit,
            });
        }

        let fields: Vec<(String, Value)> = row
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect();

        let mut seen = HashSet::with_capacity(fields.len());
        for (name, _) in &fields {
            if !seen.insert(name.as_str()) {
                return Err(AppendError::DuplicateField {
                    field: name.clone(),
                });
            }
        }

        let missing: Vec<String> = self
            .schema
            .fields()
            .iter()
            .filter(|field| !seen.contains(field.name()))
            .map(|field| field.name().to_owned())
            .collect();
        let unexpected: Vec<String> = fields
            .iter()
            .filter(|(name, _)| self.schema.index_of(name).is_none())
            .map(|(name, _)| name.clone())
            .collect();

        if !missing.is_empty() || !unexpected.is_empty() {
            return Err(AppendError::RowShapeMismatch {
                expected: self.schema.len(),
                actual: fields.len(),
                missing,
                unexpected,
            });
        }

        for (name, value) in &fields {
            let field = self
                .schema
                .field(name)
                .expect("row shape was validated against the schema");
            if matches!(value, Value::Null) {
                if !field.is_nullable() {
                    return Err(AppendError::NullabilityViolation {
                        field: name.clone(),
                    });
                }
            } else if !value_matches_type(value, field.data_type()) {
                return Err(AppendError::TypeMismatch {
                    field: name.clone(),
                    expected: field.data_type(),
                    actual: value.value_type(),
                });
            }
        }

        let mut ordered: Vec<Option<Value>> = std::iter::repeat_with(|| None)
            .take(self.columns.len())
            .collect();
        for (name, value) in fields {
            let index = self
                .schema
                .index_of(&name)
                .expect("row shape was validated against the schema");
            ordered[index] = Some(value);
        }

        for (column, value) in self.columns.iter_mut().zip(ordered) {
            column.push_validated(value.expect("all schema fields were provided"));
        }
        self.row_count += 1;
        Ok(())
    }

    /// Returns the table schema.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Returns the columns in schema order.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns a column by field name.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.schema.index_of(name).map(|index| &self.columns[index])
    }

    /// Returns the current number of rows.
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the configured maximum number of rows.
    pub fn row_limit(&self) -> usize {
        self.row_limit
    }
}

fn value_matches_type(value: &Value, data_type: DataType) -> bool {
    matches!(
        (value, data_type),
        (Value::Int64(_), DataType::Int64)
            | (Value::Float64(_), DataType::Float64)
            | (Value::Bool(_), DataType::Bool)
            | (Value::String(_), DataType::String)
    )
}

/// A validation error returned by [`Table::append_row`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppendError {
    /// The row would exceed the table's configured row bound.
    RowLimitExceeded {
        /// The configured maximum number of rows.
        limit: usize,
    },
    /// A field occurs more than once in the input row.
    DuplicateField {
        /// The repeated field name.
        field: String,
    },
    /// The row's field names differ from the schema.
    RowShapeMismatch {
        /// The number of schema fields.
        expected: usize,
        /// The number of fields supplied by the row.
        actual: usize,
        /// Schema fields not supplied by the row, in schema order.
        missing: Vec<String>,
        /// Row fields absent from the schema, in input order.
        unexpected: Vec<String>,
    },
    /// A non-null value does not have its field's declared type.
    TypeMismatch {
        /// The field containing the invalid value.
        field: String,
        /// The field's declared type.
        expected: DataType,
        /// The supplied value's runtime type.
        actual: ValueType,
    },
    /// A null was supplied for a non-nullable field.
    NullabilityViolation {
        /// The non-nullable field name.
        field: String,
    },
}

impl fmt::Display for AppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowLimitExceeded { limit } => {
                write!(formatter, "row limit of {limit} has been reached")
            }
            Self::DuplicateField { field } => {
                write!(formatter, "row contains duplicate field `{field}`")
            }
            Self::RowShapeMismatch {
                expected,
                actual,
                missing,
                unexpected,
            } => write!(
                formatter,
                "row shape mismatch: expected {expected} fields, got {actual}; missing {missing:?}; unexpected {unexpected:?}"
            ),
            Self::TypeMismatch {
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "field `{field}` expects {expected}, but received {actual}"
            ),
            Self::NullabilityViolation { field } => {
                write!(formatter, "field `{field}` is not nullable")
            }
        }
    }
}

impl Error for AppendError {}
