use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// A physical value type supported by RustHouse's in-memory storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// A signed 64-bit integer stored in a `Vec<i64>`.
    Int64,
    /// An IEEE 754 double-precision value stored in a `Vec<f64>`.
    Float64,
    /// A boolean value stored in a `Vec<bool>`.
    Bool,
    /// UTF-8 text stored in a `Vec<String>`.
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

/// The name and physical type of one table column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
}

impl ColumnSchema {
    /// Defines a column. Name uniqueness is checked by [`Schema::new`].
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

    /// Returns the physical type accepted by this column.
    pub fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// An ordered, validated collection of column definitions.
///
/// Column names are case-sensitive and must be unique. Column order determines
/// the required order of values passed to `Table::append_row`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    columns: Vec<ColumnSchema>,
}

impl Schema {
    /// Creates a schema, rejecting the first duplicate column name.
    pub fn new(columns: Vec<ColumnSchema>) -> Result<Self, SchemaError> {
        let mut names = HashSet::with_capacity(columns.len());
        for column in &columns {
            if !names.insert(column.name()) {
                return Err(SchemaError::DuplicateColumnName {
                    name: column.name().to_owned(),
                });
            }
        }

        Ok(Self { columns })
    }

    /// Returns the ordered column definitions.
    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    /// Returns the number of columns in this schema.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns whether this schema has no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// An error found while defining a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// Two columns use the same case-sensitive name.
    DuplicateColumnName {
        /// The repeated column name.
        name: String,
    },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateColumnName { name } => {
                write!(formatter, "duplicate column name: {name}")
            }
        }
    }
}

impl Error for SchemaError {}
