//! Typed, nullable, in-memory columnar storage.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// A physical and logical column type supported by [`Table`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
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

/// The schema metadata for one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
    nullable: bool,
}

impl ColumnSchema {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn data_type(&self) -> DataType {
        self.data_type
    }

    pub fn is_nullable(&self) -> bool {
        self.nullable
    }
}

/// An invalid table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    Empty,
    EmptyColumnName { column: usize },
    DuplicateColumnName { name: String },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a table schema must contain at least one column"),
            Self::EmptyColumnName { column } => {
                write!(formatter, "column {column} has an empty name")
            }
            Self::DuplicateColumnName { name } => {
                write!(formatter, "duplicate column name {name:?}")
            }
        }
    }
}

impl Error for SchemaError {}

/// An ordered, uniquely named set of table columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    columns: Vec<ColumnSchema>,
}

impl Schema {
    pub fn new(columns: Vec<ColumnSchema>) -> Result<Self, SchemaError> {
        if columns.is_empty() {
            return Err(SchemaError::Empty);
        }

        let mut names = HashSet::with_capacity(columns.len());
        for (index, column) in columns.iter().enumerate() {
            if column.name.is_empty() {
                return Err(SchemaError::EmptyColumnName { column: index });
            }
            if !names.insert(column.name.as_str()) {
                return Err(SchemaError::DuplicateColumnName {
                    name: column.name.clone(),
                });
            }
        }

        Ok(Self { columns })
    }

    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.columns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn column(&self, index: usize) -> Option<&ColumnSchema> {
        self.columns.get(index)
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }
}

/// An owned value accepted by [`Table::insert_row`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Value {
    pub fn value_type(&self) -> ValueType {
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

/// A borrowed value returned by column and row reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueRef<'a> {
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(&'a str),
}

impl ValueRef<'_> {
    pub fn to_owned(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Int64(value) => Value::Int64(value),
            Self::Float64(value) => Value::Float64(value),
            Self::Bool(value) => Value::Bool(value),
            Self::String(value) => Value::String(value.to_owned()),
        }
    }
}

/// The discriminant of an input [`Value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Null,
    Int64,
    Float64,
    Bool,
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

#[derive(Debug, Clone, PartialEq)]
struct NullBitmap {
    words: Vec<u64>,
    len: usize,
    null_count: usize,
}

impl NullBitmap {
    fn new() -> Self {
        Self {
            words: Vec::new(),
            len: 0,
            null_count: 0,
        }
    }

    fn push(&mut self, is_null: bool) {
        let bit = self.len % u64::BITS as usize;
        if bit == 0 {
            self.words.push(0);
        }
        if is_null {
            let word = self.len / u64::BITS as usize;
            self.words[word] |= 1 << bit;
            self.null_count += 1;
        }
        self.len += 1;
    }

    fn is_null(&self, index: usize) -> bool {
        debug_assert!(index < self.len);
        let word = index / u64::BITS as usize;
        let bit = index % u64::BITS as usize;
        self.words[word] & (1 << bit) != 0
    }
}

/// Contiguous values and an optional packed null bitmap for one physical type.
///
/// Nullable columns retain a placeholder in `values` for each NULL, keeping all
/// physical columns aligned by row. Bit `n` in [`Self::null_bitmap_words`] is set
/// when row `n` is NULL.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedColumn<T> {
    values: Vec<T>,
    nulls: Option<NullBitmap>,
}

impl<T> TypedColumn<T> {
    fn new(nullable: bool) -> Self {
        Self {
            values: Vec::new(),
            nulls: nullable.then(NullBitmap::new),
        }
    }

    fn push(&mut self, value: T, is_null: bool) {
        debug_assert!(self.nulls.is_some() || !is_null);
        self.values.push(value);
        if let Some(nulls) = &mut self.nulls {
            nulls.push(is_null);
        }
    }

    pub fn values(&self) -> &[T] {
        &self.values
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn is_nullable(&self) -> bool {
        self.nulls.is_some()
    }

    pub fn null_count(&self) -> usize {
        self.nulls.as_ref().map_or(0, |nulls| nulls.null_count)
    }

    pub fn is_null(&self, index: usize) -> Option<bool> {
        self.values.get(index)?;
        Some(
            self.nulls
                .as_ref()
                .is_some_and(|nulls| nulls.is_null(index)),
        )
    }

    pub fn get(&self, index: usize) -> Option<Option<&T>> {
        let value = self.values.get(index)?;
        Some((self.is_null(index) == Some(false)).then_some(value))
    }

    pub fn null_bitmap_words(&self) -> Option<&[u64]> {
        self.nulls.as_ref().map(|nulls| nulls.words.as_slice())
    }
}

pub type Int64Column = TypedColumn<i64>;
pub type Float64Column = TypedColumn<f64>;
pub type BoolColumn = TypedColumn<bool>;
pub type StringColumn = TypedColumn<String>;

/// A physical column whose variant matches its schema type.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Int64(Int64Column),
    Float64(Float64Column),
    Bool(BoolColumn),
    String(StringColumn),
}

impl Column {
    fn new(schema: &ColumnSchema) -> Self {
        match schema.data_type {
            DataType::Int64 => Self::Int64(TypedColumn::new(schema.nullable)),
            DataType::Float64 => Self::Float64(TypedColumn::new(schema.nullable)),
            DataType::Bool => Self::Bool(TypedColumn::new(schema.nullable)),
            DataType::String => Self::String(TypedColumn::new(schema.nullable)),
        }
    }

    fn push(&mut self, value: &Value) {
        match (self, value) {
            (Self::Int64(column), Value::Int64(value)) => column.push(*value, false),
            (Self::Int64(column), Value::Null) => column.push(0, true),
            (Self::Float64(column), Value::Float64(value)) => column.push(*value, false),
            (Self::Float64(column), Value::Null) => column.push(0.0, true),
            (Self::Bool(column), Value::Bool(value)) => column.push(*value, false),
            (Self::Bool(column), Value::Null) => column.push(false, true),
            (Self::String(column), Value::String(value)) => column.push(value.clone(), false),
            (Self::String(column), Value::Null) => column.push(String::new(), true),
            _ => unreachable!("row values are validated before insertion"),
        }
    }

    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Int64(column) => column.len(),
            Self::Float64(column) => column.len(),
            Self::Bool(column) => column.len(),
            Self::String(column) => column.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_nullable(&self) -> bool {
        match self {
            Self::Int64(column) => column.is_nullable(),
            Self::Float64(column) => column.is_nullable(),
            Self::Bool(column) => column.is_nullable(),
            Self::String(column) => column.is_nullable(),
        }
    }

    pub fn null_count(&self) -> usize {
        match self {
            Self::Int64(column) => column.null_count(),
            Self::Float64(column) => column.null_count(),
            Self::Bool(column) => column.null_count(),
            Self::String(column) => column.null_count(),
        }
    }

    pub fn get(&self, index: usize) -> Option<ValueRef<'_>> {
        match self {
            Self::Int64(column) => column
                .get(index)
                .map(|value| value.map_or(ValueRef::Null, |value| ValueRef::Int64(*value))),
            Self::Float64(column) => column
                .get(index)
                .map(|value| value.map_or(ValueRef::Null, |value| ValueRef::Float64(*value))),
            Self::Bool(column) => column
                .get(index)
                .map(|value| value.map_or(ValueRef::Null, |value| ValueRef::Bool(*value))),
            Self::String(column) => column.get(index).map(|value| {
                value.map_or(ValueRef::Null, |value| ValueRef::String(value.as_str()))
            }),
        }
    }
}

/// Configurable resource limits for an in-memory table.
///
/// `max_string_bytes` bounds the total UTF-8 payload stored by all String
/// columns. NULL placeholders do not consume this budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableLimits {
    pub max_rows: usize,
    pub max_string_bytes: usize,
}

impl TableLimits {
    pub const DEFAULT_MAX_ROWS: usize = 1_000_000;
    pub const DEFAULT_MAX_STRING_BYTES: usize = 64 * 1024 * 1024;

    pub const fn new(max_rows: usize, max_string_bytes: usize) -> Self {
        Self {
            max_rows,
            max_string_bytes,
        }
    }
}

impl Default for TableLimits {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_ROWS, Self::DEFAULT_MAX_STRING_BYTES)
    }
}

/// A stable classification for a rejected non-finite `Float64` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonFiniteFloat {
    NaN,
    PositiveInfinity,
    NegativeInfinity,
}

impl NonFiniteFloat {
    fn from_value(value: f64) -> Self {
        if value.is_nan() {
            Self::NaN
        } else if value.is_sign_positive() {
            Self::PositiveInfinity
        } else {
            Self::NegativeInfinity
        }
    }
}

impl fmt::Display for NonFiniteFloat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NaN => "NaN",
            Self::PositiveInfinity => "+infinity",
            Self::NegativeInfinity => "-infinity",
        })
    }
}

/// A row rejected before any physical column was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertError {
    Shape {
        expected: usize,
        actual: usize,
    },
    TypeMismatch {
        column: usize,
        column_name: String,
        expected: DataType,
        actual: ValueType,
    },
    NullNotAllowed {
        column: usize,
        column_name: String,
    },
    NonFiniteFloat {
        column: usize,
        column_name: String,
        value: NonFiniteFloat,
    },
    RowLimitExceeded {
        limit: usize,
    },
    StringLimitExceeded {
        limit: usize,
        current: usize,
        attempted: usize,
    },
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shape { expected, actual } => {
                write!(
                    formatter,
                    "row has {actual} values but schema requires {expected}"
                )
            }
            Self::TypeMismatch {
                column,
                column_name,
                expected,
                actual,
            } => write!(
                formatter,
                "column {column} ({column_name:?}) requires {expected}, got {actual}"
            ),
            Self::NullNotAllowed {
                column,
                column_name,
            } => write!(
                formatter,
                "column {column} ({column_name:?}) does not allow NULL"
            ),
            Self::NonFiniteFloat {
                column,
                column_name,
                value,
            } => write!(
                formatter,
                "column {column} ({column_name:?}) does not allow non-finite float {value}"
            ),
            Self::RowLimitExceeded { limit } => {
                write!(formatter, "table row limit of {limit} would be exceeded")
            }
            Self::StringLimitExceeded {
                limit,
                current,
                attempted,
            } => write!(
                formatter,
                "table string limit of {limit} bytes would be exceeded (currently {current}, attempted {attempted})"
            ),
        }
    }
}

impl Error for InsertError {}

/// An in-memory table backed by one contiguous vector per schema column.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    limits: TableLimits,
    row_count: usize,
    string_bytes: usize,
}

impl Table {
    pub fn new(schema: Schema) -> Self {
        Self::with_limits(schema, TableLimits::default())
    }

    pub fn with_limits(schema: Schema, limits: TableLimits) -> Self {
        let columns = schema.columns.iter().map(Column::new).collect();
        Self {
            schema,
            columns,
            limits,
            row_count: 0,
            string_bytes: 0,
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn limits(&self) -> TableLimits {
        self.limits
    }

    pub fn len(&self) -> usize {
        self.row_count
    }

    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    pub fn string_bytes(&self) -> usize {
        self.string_bytes
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    pub fn column_by_name(&self, name: &str) -> Option<&Column> {
        self.schema
            .index_of(name)
            .and_then(|index| self.column(index))
    }

    pub fn get(&self, row: usize, column: usize) -> Option<ValueRef<'_>> {
        self.column(column)?.get(row)
    }

    pub fn row(&self, index: usize) -> Option<Vec<ValueRef<'_>>> {
        if index >= self.row_count {
            return None;
        }
        self.columns
            .iter()
            .map(|column| column.get(index))
            .collect()
    }

    /// Validates and appends one row. Every fallible validation completes before
    /// any column is changed, so every returned error leaves the table unchanged.
    pub fn insert_row(&mut self, values: &[Value]) -> Result<(), InsertError> {
        if values.len() != self.schema.len() {
            return Err(InsertError::Shape {
                expected: self.schema.len(),
                actual: values.len(),
            });
        }
        if self.row_count >= self.limits.max_rows {
            return Err(InsertError::RowLimitExceeded {
                limit: self.limits.max_rows,
            });
        }

        let mut added_string_bytes = 0usize;
        for (index, (column, value)) in self.schema.columns.iter().zip(values).enumerate() {
            match value {
                Value::Null if !column.nullable => {
                    return Err(InsertError::NullNotAllowed {
                        column: index,
                        column_name: column.name.clone(),
                    });
                }
                Value::Null => {}
                Value::Float64(value) if column.data_type == DataType::Float64 => {
                    if !value.is_finite() {
                        return Err(InsertError::NonFiniteFloat {
                            column: index,
                            column_name: column.name.clone(),
                            value: NonFiniteFloat::from_value(*value),
                        });
                    }
                }
                Value::String(value) if column.data_type == DataType::String => {
                    added_string_bytes = added_string_bytes.saturating_add(value.len());
                }
                value if value.value_type() != ValueType::from(column.data_type) => {
                    return Err(InsertError::TypeMismatch {
                        column: index,
                        column_name: column.name.clone(),
                        expected: column.data_type,
                        actual: value.value_type(),
                    });
                }
                _ => {}
            }
        }

        let attempted_string_bytes = self.string_bytes.saturating_add(added_string_bytes);
        if attempted_string_bytes > self.limits.max_string_bytes {
            return Err(InsertError::StringLimitExceeded {
                limit: self.limits.max_string_bytes,
                current: self.string_bytes,
                attempted: attempted_string_bytes,
            });
        }

        for (column, value) in self.columns.iter_mut().zip(values) {
            column.push(value);
        }
        self.row_count += 1;
        self.string_bytes = attempted_string_bytes;
        Ok(())
    }
}

impl From<DataType> for ValueType {
    fn from(value: DataType) -> Self {
        match value {
            DataType::Int64 => Self::Int64,
            DataType::Float64 => Self::Float64,
            DataType::Bool => Self::Bool,
            DataType::String => Self::String,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_bitmap_crosses_word_boundaries() {
        let mut column = TypedColumn::new(true);
        for index in 0..65 {
            column.push(index, index == 0 || index == 63 || index == 64);
        }

        assert_eq!(column.null_count(), 3);
        assert_eq!(column.null_bitmap_words(), Some(&[(1 << 63) | 1, 1][..]));
        assert_eq!(column.get(0), Some(None));
        assert_eq!(column.get(1), Some(Some(&1)));
        assert_eq!(column.get(63), Some(None));
        assert_eq!(column.get(64), Some(None));
        assert_eq!(column.get(65), None);
    }
}
