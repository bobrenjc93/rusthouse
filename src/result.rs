/// Logical type of a result column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// Signed 64-bit integer.
    Int64,
    /// Finite 64-bit floating-point value.
    Float64,
    /// Boolean value.
    Bool,
    /// UTF-8 string.
    String,
}

/// A scalar value in a query result.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Signed 64-bit integer.
    Int64(i64),
    /// Finite 64-bit floating-point value.
    Float64(f64),
    /// Boolean value.
    Bool(bool),
    /// UTF-8 string.
    String(String),
}

impl Value {
    /// Returns the logical type of this value.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    pub(crate) fn display_value(&self) -> String {
        match self {
            Self::Int64(value) => value.to_string(),
            Self::Float64(value) => value.to_string(),
            Self::Bool(value) => value.to_string(),
            Self::String(value) => value.clone(),
        }
    }
}

/// Metadata for one result column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Column name or query alias.
    pub name: String,
    /// Logical type shared by values in the column.
    pub data_type: DataType,
}

/// Materialized query result in row-major form.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Ordered column metadata.
    pub columns: Vec<Column>,
    /// Materialized rows. Each row has the same length as [`Self::columns`].
    pub rows: Vec<Vec<Value>>,
}

impl QueryResult {
    pub(crate) fn single(name: String, value: Value) -> Self {
        let data_type = value.data_type();
        Self {
            columns: vec![Column { name, data_type }],
            rows: vec![vec![value]],
        }
    }
}
