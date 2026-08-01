use crate::error::{Error, Result};
use crate::{DataType, Value};

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

    fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    fn has_null(&self) -> bool {
        match self {
            Self::Int64(values) => values.iter().any(Option::is_none),
            Self::Float64(values) => values.iter().any(Option::is_none),
            Self::Bool(values) => values.iter().any(Option::is_none),
            Self::String(values) => values.iter().any(Option::is_none),
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

    fn owned_value_bytes(&self, index: usize) -> usize {
        std::mem::size_of::<Value>()
            + match self {
                Self::String(values) => values[index].as_ref().map_or(0, String::len),
                _ => 0,
            }
    }

    fn logical_value_bytes(&self, index: usize) -> usize {
        match self {
            Self::Int64(values) => values[index].map_or(0, |_| 8),
            Self::Float64(values) => values[index].map_or(0, |_| 8),
            Self::Bool(values) => values[index].map_or(0, |_| 1),
            Self::String(values) => values[index].as_ref().map_or(0, String::len),
        }
    }

    fn logical_bytes(&self) -> usize {
        match self {
            Self::Int64(values) => values
                .len()
                .saturating_mul(std::mem::size_of::<Option<i64>>()),
            Self::Float64(values) => values
                .len()
                .saturating_mul(std::mem::size_of::<Option<f64>>()),
            Self::Bool(values) => values
                .len()
                .saturating_mul(std::mem::size_of::<Option<bool>>()),
            Self::String(values) => values.iter().fold(
                values
                    .len()
                    .saturating_mul(std::mem::size_of::<Option<String>>()),
                |bytes, value| bytes.saturating_add(value.as_ref().map_or(0, String::len)),
            ),
        }
    }
}

/// An immutable-on-commit, typed columnar table.
#[derive(Debug, Clone)]
pub struct Table {
    schema: Vec<ColumnDef>,
    columns: Vec<ColumnData>,
    row_count: usize,
    logical_bytes: usize,
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
        let logical_bytes = schema_logical_bytes(&schema);
        Ok(Self {
            schema,
            columns,
            row_count: 0,
            logical_bytes,
        })
    }

    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub(crate) fn logical_bytes(&self) -> usize {
        self.logical_bytes
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
        let additional_logical_bytes = rows.iter().fold(0_usize, |bytes, row| {
            bytes.saturating_add(row_logical_bytes(&self.schema, row))
        });
        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count = row_count;
        self.logical_bytes = self.logical_bytes.saturating_add(additional_logical_bytes);
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

    pub(crate) fn owned_value_bytes(&self, row: usize, column: usize) -> usize {
        self.columns[column].owned_value_bytes(row)
    }

    pub(crate) fn logical_value_bytes(&self, row: usize, column: usize) -> usize {
        self.columns[column].logical_value_bytes(row)
    }

    pub(crate) fn columns(&self) -> &[ColumnData] {
        &self.columns
    }

    pub(crate) fn from_parts(schema: Vec<ColumnDef>, columns: Vec<ColumnData>) -> Result<Self> {
        if schema.is_empty() || schema.len() != columns.len() {
            return Err(Error::CorruptSnapshot(
                "table columns do not match its schema".to_owned(),
            ));
        }
        let row_count = columns.first().map_or(0, ColumnData::len);
        for (index, (definition, data)) in schema.iter().zip(&columns).enumerate() {
            if definition.name.is_empty()
                || schema[..index]
                    .iter()
                    .any(|existing| existing.name == definition.name)
            {
                return Err(Error::CorruptSnapshot(
                    "table schema contains an empty or duplicate column name".to_owned(),
                ));
            }
            if data.len() != row_count || data.data_type() != definition.data_type {
                return Err(Error::CorruptSnapshot(
                    "table columns do not match its schema or row count".to_owned(),
                ));
            }
            if !definition.nullable && data.has_null() {
                return Err(Error::CorruptSnapshot(
                    "non-nullable column contains NULL".to_owned(),
                ));
            }
        }
        let logical_bytes = schema_logical_bytes(&schema).saturating_add(
            columns.iter().fold(0_usize, |bytes, column| {
                bytes.saturating_add(column.logical_bytes())
            }),
        );
        Ok(Self {
            schema,
            columns,
            row_count,
            logical_bytes,
        })
    }
}

fn schema_logical_bytes(schema: &[ColumnDef]) -> usize {
    schema.iter().fold(0_usize, |bytes, column| {
        bytes
            .saturating_add(std::mem::size_of::<ColumnDef>())
            .saturating_add(column.name.len())
    })
}

fn row_logical_bytes(schema: &[ColumnDef], row: &[Value]) -> usize {
    schema
        .iter()
        .zip(row)
        .fold(0_usize, |bytes, (column, value)| {
            let fixed = match column.data_type {
                DataType::Int64 => std::mem::size_of::<Option<i64>>(),
                DataType::Float64 => std::mem::size_of::<Option<f64>>(),
                DataType::Bool => std::mem::size_of::<Option<bool>>(),
                DataType::String => std::mem::size_of::<Option<String>>(),
            };
            let owned = match value {
                Value::String(value) => value.len(),
                _ => 0,
            };
            bytes.saturating_add(fixed).saturating_add(owned)
        })
}
