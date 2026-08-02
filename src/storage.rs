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

/// Maximum number of fields in a [`Schema`].
pub const MAX_SCHEMA_FIELDS: usize = 1_024;

/// Maximum UTF-8 byte length of a schema field identifier.
pub const MAX_IDENTIFIER_BYTES: usize = 256;

/// Maximum UTF-8 byte length of one stored [`Value::String`].
pub const MAX_STORED_STRING_BYTES: usize = 1024 * 1024;

/// Hard upper bound for accounted column data retained by one [`Table`].
///
/// The 256 MiB budget includes each typed value slot, string allocation
/// capacity, and one conservative byte per validity bit. Schema metadata and
/// allocator bookkeeping are not included.
pub const MAX_TABLE_DATA_BYTES: usize = 256 * 1024 * 1024;

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
    /// Builds a schema, enforcing field-count, identifier-size, and uniqueness
    /// constraints.
    pub fn new(fields: Vec<Field>) -> Result<Self, SchemaError> {
        if fields.len() > MAX_SCHEMA_FIELDS {
            return Err(SchemaError::TooManyFields {
                limit: MAX_SCHEMA_FIELDS,
                actual: fields.len(),
            });
        }

        let mut field_indexes = HashMap::with_capacity(fields.len());
        for (index, field) in fields.iter().enumerate() {
            if field.name.len() > MAX_IDENTIFIER_BYTES {
                return Err(SchemaError::IdentifierTooLong {
                    field: field.name.clone(),
                    length: field.name.len(),
                    limit: MAX_IDENTIFIER_BYTES,
                });
            }
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
    /// The schema contains more fields than the storage limit permits.
    TooManyFields {
        /// The maximum number of fields.
        limit: usize,
        /// The number of supplied fields.
        actual: usize,
    },
    /// A field identifier exceeds the UTF-8 byte-length limit.
    IdentifierTooLong {
        /// The oversized field identifier.
        field: String,
        /// The identifier's UTF-8 byte length.
        length: usize,
        /// The maximum UTF-8 byte length.
        limit: usize,
    },
    /// Two schema fields have the same name.
    DuplicateField {
        /// The repeated field name.
        field: String,
    },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyFields { limit, actual } => {
                write!(
                    formatter,
                    "schema has {actual} fields, exceeding the limit of {limit}"
                )
            }
            Self::IdentifierTooLong {
                field,
                length,
                limit,
            } => write!(
                formatter,
                "schema field `{field}` is {length} UTF-8 bytes, exceeding the limit of {limit}"
            ),
            Self::DuplicateField { field } => {
                write!(formatter, "schema contains duplicate field `{field}`")
            }
        }
    }
}

impl Error for SchemaError {}

/// A scalar value accepted by [`Table::append_row`] and [`Table::append_batch`].
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

    fn append(&mut self, other: Self) {
        for index in 0..other.len {
            self.push(
                other
                    .get(index)
                    .expect("the appended bitmap index is in bounds"),
            );
        }
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

    fn append(&mut self, other: Self) {
        self.values.extend(other.values);
        self.validity.append(other.validity);
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

    fn append(&mut self, other: Self) {
        match (self, other) {
            (Self::Int64(column), Self::Int64(other)) => column.append(other),
            (Self::Float64(column), Self::Float64(other)) => column.append(other),
            (Self::Bool(column), Self::Bool(other)) => column.append(other),
            (Self::String(column), Self::String(other)) => column.append(other),
            _ => unreachable!("validated table deltas have matching column types"),
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
    data_size_bytes: usize,
    data_byte_limit: usize,
}

impl Table {
    /// Creates an empty table bounded by `row_limit` and
    /// [`MAX_TABLE_DATA_BYTES`].
    pub fn new(schema: Schema, row_limit: usize) -> Self {
        Self::with_data_limit(schema, row_limit, MAX_TABLE_DATA_BYTES)
    }

    /// Creates an empty table with row and accounted-data bounds.
    ///
    /// `data_byte_limit` can lower the global [`MAX_TABLE_DATA_BYTES`] bound but
    /// cannot raise it. The effective value is available through
    /// [`Table::data_byte_limit`].
    pub fn with_data_limit(schema: Schema, row_limit: usize, data_byte_limit: usize) -> Self {
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
            data_size_bytes: 0,
            data_byte_limit: data_byte_limit.min(MAX_TABLE_DATA_BYTES),
        }
    }

    /// Atomically appends a named row after validating its complete contents.
    ///
    /// Field order does not matter. Duplicate fields, missing or unexpected
    /// fields, type mismatches, disallowed nulls, oversized strings, and rows
    /// beyond the configured limit return an [`AppendError`] without changing
    /// the table.
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

        // One value beyond the schema width is enough to reject an oversized
        // row without exhausting an untrusted or infinite iterator.
        let field_limit = self.schema.len().saturating_add(1);
        let fields: Vec<(String, Value)> = row
            .into_iter()
            .take(field_limit)
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
            } else if let Some(length) = oversized_string_length(value) {
                return Err(AppendError::StringTooLong {
                    field: name.clone(),
                    length,
                    limit: MAX_STORED_STRING_BYTES,
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

        let row_data_size = accounted_row_bytes(
            &self.schema,
            ordered
                .iter()
                .map(|value| value.as_ref().expect("all schema fields were provided")),
        );
        let attempted = self.data_size_bytes.saturating_add(row_data_size);
        if attempted > self.data_byte_limit {
            return Err(AppendError::TableDataLimitExceeded {
                attempted,
                limit: self.data_byte_limit,
            });
        }

        for (column, value) in self.columns.iter_mut().zip(ordered) {
            column.push_validated(value.expect("all schema fields were provided"));
        }
        self.row_count += 1;
        self.data_size_bytes = attempted;
        Ok(())
    }

    /// Atomically appends schema-ordered rows after validating the entire batch.
    ///
    /// Each row must contain exactly one [`Value`] per schema field, in schema
    /// order. An empty batch succeeds, including when the table is full. Any
    /// shape, type, nullability, string-size, or row-limit error identifies its
    /// zero-based row within the batch and leaves the table unchanged.
    ///
    /// Consumption is bounded for untrusted iterators: at most one row beyond
    /// the remaining table capacity and one value beyond the schema width are
    /// requested.
    pub fn append_batch<I, R>(&mut self, rows: I) -> Result<(), BatchAppendError>
    where
        I: IntoIterator<Item = R>,
        R: IntoIterator<Item = Value>,
    {
        self.append_batch_after(rows, 0, self.row_limit, 0, self.data_byte_limit)
    }

    pub(crate) fn append_batch_after<I, R>(
        &mut self,
        rows: I,
        base_row_count: usize,
        row_limit: usize,
        base_data_size_bytes: usize,
        data_byte_limit: usize,
    ) -> Result<(), BatchAppendError>
    where
        I: IntoIterator<Item = R>,
        R: IntoIterator<Item = Value>,
    {
        let logical_row_count = base_row_count.saturating_add(self.row_count);
        let remaining = row_limit.saturating_sub(logical_row_count);
        let consumption_limit = remaining.saturating_add(1);
        let value_limit = self.schema.len().saturating_add(1);
        let mut validated_rows = Vec::new();
        let mut appended_data_size = 0_usize;

        for (row_index, row) in rows.into_iter().take(consumption_limit).enumerate() {
            if row_index == remaining {
                return Err(BatchAppendError::RowLimitExceeded {
                    row_index,
                    limit: row_limit,
                });
            }

            let values: Vec<Value> = row.into_iter().take(value_limit).collect();
            if values.len() != self.schema.len() {
                return Err(BatchAppendError::RowShapeMismatch {
                    row_index,
                    expected: self.schema.len(),
                    actual: values.len(),
                });
            }

            for (field, value) in self.schema.fields().iter().zip(&values) {
                if matches!(value, Value::Null) {
                    if !field.is_nullable() {
                        return Err(BatchAppendError::NullabilityViolation {
                            row_index,
                            field: field.name().to_owned(),
                        });
                    }
                } else if !value_matches_type(value, field.data_type()) {
                    return Err(BatchAppendError::TypeMismatch {
                        row_index,
                        field: field.name().to_owned(),
                        expected: field.data_type(),
                        actual: value.value_type(),
                    });
                } else if let Some(length) = oversized_string_length(value) {
                    return Err(BatchAppendError::StringTooLong {
                        row_index,
                        field: field.name().to_owned(),
                        length,
                        limit: MAX_STORED_STRING_BYTES,
                    });
                }
            }

            let row_data_size = accounted_row_bytes(&self.schema, values.iter());
            let attempted = base_data_size_bytes
                .saturating_add(self.data_size_bytes)
                .saturating_add(appended_data_size)
                .saturating_add(row_data_size);
            if attempted > data_byte_limit {
                return Err(BatchAppendError::TableDataLimitExceeded {
                    row_index,
                    attempted,
                    limit: data_byte_limit,
                });
            }
            appended_data_size = appended_data_size.saturating_add(row_data_size);

            validated_rows.push(values);
        }

        let appended = validated_rows.len();
        for row in validated_rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push_validated(value);
            }
        }
        self.row_count += appended;
        self.data_size_bytes = self.data_size_bytes.saturating_add(appended_data_size);
        Ok(())
    }

    pub(crate) fn append_committed(&mut self, delta: Self) {
        debug_assert_eq!(self.schema, delta.schema);
        debug_assert!(self.row_count.saturating_add(delta.row_count) <= self.row_limit);
        debug_assert!(
            self.data_size_bytes.saturating_add(delta.data_size_bytes) <= self.data_byte_limit
        );

        for (column, delta_column) in self.columns.iter_mut().zip(delta.columns) {
            column.append(delta_column);
        }
        self.row_count += delta.row_count;
        self.data_size_bytes += delta.data_size_bytes;
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

    /// Returns the accounted bytes retained by typed values and validity data.
    pub fn data_size_bytes(&self) -> usize {
        self.data_size_bytes
    }

    /// Returns the effective accounted-data limit for this table.
    pub fn data_byte_limit(&self) -> usize {
        self.data_byte_limit
    }
}

fn accounted_row_bytes<'a>(schema: &Schema, values: impl IntoIterator<Item = &'a Value>) -> usize {
    schema
        .fields()
        .iter()
        .zip(values)
        .fold(schema.len(), |total, (field, value)| {
            let slot_bytes = match field.data_type() {
                DataType::Int64 => std::mem::size_of::<i64>(),
                DataType::Float64 => std::mem::size_of::<f64>(),
                DataType::Bool => std::mem::size_of::<bool>(),
                DataType::String => {
                    std::mem::size_of::<String>()
                        + match value {
                            Value::String(value) => value.capacity(),
                            Value::Null => 0,
                            _ => unreachable!("row values are type-checked before accounting"),
                        }
                }
            };
            total.saturating_add(slot_bytes)
        })
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

fn oversized_string_length(value: &Value) -> Option<usize> {
    match value {
        Value::String(value) if value.len() > MAX_STORED_STRING_BYTES => Some(value.len()),
        _ => None,
    }
}

/// A validation error returned by [`Table::append_row`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppendError {
    /// The row would exceed the table's configured row bound.
    RowLimitExceeded {
        /// The configured maximum number of rows.
        limit: usize,
    },
    /// The row would exceed the table's accounted column-data budget.
    TableDataLimitExceeded {
        /// Accounted bytes the table would retain after the append.
        attempted: usize,
        /// The configured maximum accounted bytes.
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
        /// The number of fields observed. When this is greater than `expected`,
        /// it is a lower bound because ingestion stops after `expected + 1`.
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
    /// A string value exceeds the stored UTF-8 byte-length limit.
    StringTooLong {
        /// The field containing the oversized string.
        field: String,
        /// The string's UTF-8 byte length.
        length: usize,
        /// The maximum UTF-8 byte length.
        limit: usize,
    },
}

impl fmt::Display for AppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowLimitExceeded { limit } => {
                write!(formatter, "row limit of {limit} has been reached")
            }
            Self::TableDataLimitExceeded { attempted, limit } => write!(
                formatter,
                "table data would require {attempted} bytes, exceeding the limit of {limit}"
            ),
            Self::DuplicateField { field } => {
                write!(formatter, "row contains duplicate field `{field}`")
            }
            Self::RowShapeMismatch {
                expected,
                actual,
                missing,
                unexpected,
            } => {
                let qualifier = if actual > expected { "at least " } else { "" };
                write!(
                    formatter,
                    "row shape mismatch: expected {expected} fields, got {qualifier}{actual}; missing {missing:?}; unexpected {unexpected:?}"
                )
            }
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
            Self::StringTooLong {
                field,
                length,
                limit,
            } => write!(
                formatter,
                "field `{field}` contains a {length}-byte string, exceeding the limit of {limit}"
            ),
        }
    }
}

impl Error for AppendError {}

/// A validation error returned by [`Table::append_batch`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchAppendError {
    /// The indexed batch row would exceed the table's configured row bound.
    RowLimitExceeded {
        /// The zero-based index of the row within the input batch.
        row_index: usize,
        /// The configured maximum number of rows.
        limit: usize,
    },
    /// The indexed batch row would exceed the table's accounted column-data
    /// budget.
    TableDataLimitExceeded {
        /// The zero-based index of the row within the input batch.
        row_index: usize,
        /// Accounted bytes the table would retain through this row.
        attempted: usize,
        /// The configured maximum accounted bytes.
        limit: usize,
    },
    /// The indexed batch row has a different width than the schema.
    RowShapeMismatch {
        /// The zero-based index of the row within the input batch.
        row_index: usize,
        /// The number of schema fields.
        expected: usize,
        /// The number of values observed. When this is greater than `expected`,
        /// it is a lower bound because ingestion stops after `expected + 1`.
        actual: usize,
    },
    /// A non-null value does not have its field's declared type.
    TypeMismatch {
        /// The zero-based index of the row within the input batch.
        row_index: usize,
        /// The field containing the invalid value.
        field: String,
        /// The field's declared type.
        expected: DataType,
        /// The supplied value's runtime type.
        actual: ValueType,
    },
    /// A null was supplied for a non-nullable field.
    NullabilityViolation {
        /// The zero-based index of the row within the input batch.
        row_index: usize,
        /// The non-nullable field name.
        field: String,
    },
    /// A string value in the indexed batch row exceeds the stored UTF-8
    /// byte-length limit.
    StringTooLong {
        /// The zero-based index of the row within the input batch.
        row_index: usize,
        /// The field containing the oversized string.
        field: String,
        /// The string's UTF-8 byte length.
        length: usize,
        /// The maximum UTF-8 byte length.
        limit: usize,
    },
}

impl BatchAppendError {
    /// Returns the zero-based index of the invalid row within the batch.
    pub fn row_index(&self) -> usize {
        match self {
            Self::RowLimitExceeded { row_index, .. }
            | Self::TableDataLimitExceeded { row_index, .. }
            | Self::RowShapeMismatch { row_index, .. }
            | Self::TypeMismatch { row_index, .. }
            | Self::NullabilityViolation { row_index, .. }
            | Self::StringTooLong { row_index, .. } => *row_index,
        }
    }

    pub(crate) fn with_row_index(self, row_index: usize) -> Self {
        match self {
            Self::RowLimitExceeded { limit, .. } => Self::RowLimitExceeded { row_index, limit },
            Self::TableDataLimitExceeded {
                attempted, limit, ..
            } => Self::TableDataLimitExceeded {
                row_index,
                attempted,
                limit,
            },
            Self::RowShapeMismatch {
                expected, actual, ..
            } => Self::RowShapeMismatch {
                row_index,
                expected,
                actual,
            },
            Self::TypeMismatch {
                field,
                expected,
                actual,
                ..
            } => Self::TypeMismatch {
                row_index,
                field,
                expected,
                actual,
            },
            Self::NullabilityViolation { field, .. } => {
                Self::NullabilityViolation { row_index, field }
            }
            Self::StringTooLong {
                field,
                length,
                limit,
                ..
            } => Self::StringTooLong {
                row_index,
                field,
                length,
                limit,
            },
        }
    }
}

impl fmt::Display for BatchAppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowLimitExceeded { row_index, limit } => write!(
                formatter,
                "batch row {row_index} exceeds the table row limit of {limit}"
            ),
            Self::TableDataLimitExceeded {
                row_index,
                attempted,
                limit,
            } => write!(
                formatter,
                "batch row {row_index} would grow table data to {attempted} bytes, exceeding the limit of {limit}"
            ),
            Self::RowShapeMismatch {
                row_index,
                expected,
                actual,
            } => {
                let qualifier = if actual > expected { "at least " } else { "" };
                write!(
                    formatter,
                    "batch row {row_index} shape mismatch: expected {expected} values, got {qualifier}{actual}"
                )
            }
            Self::TypeMismatch {
                row_index,
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "field `{field}` in batch row {row_index} expects {expected}, but received {actual}"
            ),
            Self::NullabilityViolation { row_index, field } => {
                write!(
                    formatter,
                    "field `{field}` in batch row {row_index} is not nullable"
                )
            }
            Self::StringTooLong {
                row_index,
                field,
                length,
                limit,
            } => write!(
                formatter,
                "field `{field}` in batch row {row_index} contains a {length}-byte string, exceeding the limit of {limit}"
            ),
        }
    }
}

impl Error for BatchAppendError {}
