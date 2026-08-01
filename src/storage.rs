use std::fmt;

use crate::error::{Error, Result};

/// A physical column type supported by RustHouse tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        })
    }
}

/// A scalar SQL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Value {
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Null => None,
            Self::Int64(_) => Some(DataType::Int64),
            Self::Float64(_) => Some(DataType::Float64),
            Self::Bool(_) => Some(DataType::Bool),
            Self::String(_) => Some(DataType::String),
        }
    }

    pub(crate) fn estimated_size(&self) -> usize {
        match self {
            Self::Null => 1,
            Self::Int64(_) | Self::Float64(_) => 9,
            Self::Bool(_) => 2,
            Self::String(value) => 9usize.saturating_add(value.len()),
        }
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Int64(_) => "Int64",
            Self::Float64(_) => "Float64",
            Self::Bool(_) => "Bool",
            Self::String(_) => "String",
        }
    }
}

/// A named field in a table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ColumnData {
    Int64(Vec<Option<i64>>),
    Float64(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    String(Vec<Option<String>>),
}

impl ColumnData {
    fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    fn push(&mut self, value: &Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(Some(*value)),
            (Self::Float64(values), Value::Float64(value)) => values.push(Some(*value)),
            (Self::Bool(values), Value::Bool(value)) => values.push(Some(*value)),
            (Self::String(values), Value::String(value)) => values.push(Some(value.clone())),
            (Self::Int64(values), Value::Null) => values.push(None),
            (Self::Float64(values), Value::Null) => values.push(None),
            (Self::Bool(values), Value::Null) => values.push(None),
            (Self::String(values), Value::Null) => values.push(None),
            _ => unreachable!("values are validated before they are appended"),
        }
    }

    pub(crate) fn value(&self, index: usize) -> Value {
        match self {
            Self::Int64(values) => values[index].map_or(Value::Null, Value::Int64),
            Self::Float64(values) => values[index].map_or(Value::Null, Value::Float64),
            Self::Bool(values) => values[index].map_or(Value::Null, Value::Bool),
            Self::String(values) => values[index]
                .as_ref()
                .map_or(Value::Null, |value| Value::String(value.clone())),
        }
    }
}

/// An immutable-on-commit, typed columnar table.
#[derive(Debug, Clone)]
pub struct Table {
    schema: Vec<ColumnDef>,
    columns: Vec<ColumnData>,
    row_count: usize,
}

impl Table {
    pub fn new(schema: Vec<ColumnDef>) -> Result<Self> {
        if schema.is_empty() {
            return Err(Error::InvalidRow(
                "a table must have at least one column".to_owned(),
            ));
        }
        for (index, column) in schema.iter().enumerate() {
            if column.name.is_empty() {
                return Err(Error::InvalidRow(
                    "column names must not be empty".to_owned(),
                ));
            }
            if schema[..index]
                .iter()
                .any(|existing| existing.name == column.name)
            {
                return Err(Error::DuplicateColumn(column.name.clone()));
            }
        }
        let columns = schema
            .iter()
            .map(|column| ColumnData::new(column.data_type))
            .collect();
        Ok(Self {
            schema,
            columns,
            row_count: 0,
        })
    }

    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub(crate) fn column_index(&self, name: &str) -> Option<usize> {
        self.schema.iter().position(|column| column.name == name)
    }

    pub(crate) fn append_rows(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        let row_count = self
            .row_count
            .checked_add(rows.len())
            .ok_or_else(|| Error::InvalidRow("table row count overflowed".to_owned()))?;
        for row in rows {
            self.validate_row(row)?;
        }
        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count = row_count;
        Ok(())
    }

    fn validate_row(&self, row: &[Value]) -> Result<()> {
        if row.len() != self.schema.len() {
            return Err(Error::InvalidRow(format!(
                "expected {} values, got {}",
                self.schema.len(),
                row.len()
            )));
        }
        for (column, value) in self.schema.iter().zip(row) {
            if value == &Value::Null {
                if !column.nullable {
                    return Err(Error::TypeMismatch {
                        column: column.name.clone(),
                        expected: column.data_type.to_string(),
                        actual: "NULL".to_owned(),
                    });
                }
            } else if value.data_type() != Some(column.data_type) {
                return Err(Error::TypeMismatch {
                    column: column.name.clone(),
                    expected: column.data_type.to_string(),
                    actual: value.type_name().to_owned(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn value(&self, row: usize, column: usize) -> Value {
        self.columns[column].value(row)
    }

    pub(crate) fn columns(&self) -> &[ColumnData] {
        &self.columns
    }

    pub(crate) fn from_parts(schema: Vec<ColumnDef>, columns: Vec<ColumnData>) -> Result<Self> {
        let empty = Self::new(schema.clone())?;
        let row_count = columns.first().map_or(0, ColumnData::len);
        if schema.len() != columns.len()
            || columns.iter().any(|column| column.len() != row_count)
            || empty
                .columns
                .iter()
                .zip(&columns)
                .any(|(expected, actual)| {
                    !matches!(
                        (expected, actual),
                        (ColumnData::Int64(_), ColumnData::Int64(_))
                            | (ColumnData::Float64(_), ColumnData::Float64(_))
                            | (ColumnData::Bool(_), ColumnData::Bool(_))
                            | (ColumnData::String(_), ColumnData::String(_))
                    )
                })
        {
            return Err(Error::CorruptSnapshot(
                "table columns do not match its schema or row count".to_owned(),
            ));
        }
        Ok(Self {
            schema,
            columns,
            row_count,
        })
    }
}
