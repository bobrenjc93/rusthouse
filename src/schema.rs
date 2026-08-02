use std::collections::HashSet;
use std::fmt;

use crate::{Error, Result};

/// A logical column type supported by RustHouse.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl DataType {
    pub(crate) fn parse(identifier: &str) -> Option<Self> {
        if identifier.eq_ignore_ascii_case("Int64") {
            Some(Self::Int64)
        } else if identifier.eq_ignore_ascii_case("Float64") {
            Some(Self::Float64)
        } else if identifier.eq_ignore_ascii_case("Bool") {
            Some(Self::Bool)
        } else if identifier.eq_ignore_ascii_case("String") {
            Some(Self::String)
        } else {
            None
        }
    }
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

/// A named, typed column in a table schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
}

impl ColumnSchema {
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// The validated name and ordered columns of a table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableSchema {
    name: String,
    columns: Vec<ColumnSchema>,
}

impl TableSchema {
    /// Build a schema, rejecting empty and duplicate column lists.
    pub fn new(name: impl Into<String>, columns: Vec<ColumnSchema>) -> Result<Self> {
        if columns.is_empty() {
            return Err(Error::Syntax {
                position: 0,
                message: "a table must contain at least one column".to_owned(),
            });
        }

        let mut names = HashSet::with_capacity(columns.len());
        for column in &columns {
            let normalized = column.name.to_ascii_lowercase();
            if !names.insert(normalized) {
                return Err(Error::DuplicateColumn {
                    name: column.name.clone(),
                });
            }
        }

        Ok(Self {
            name: name.into(),
            columns,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Find a column using SQL's case-insensitive identifier semantics.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&ColumnSchema> {
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
    }
}
