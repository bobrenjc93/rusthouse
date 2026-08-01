use std::mem::size_of;

use crate::batch::{
    BatchConfig, BooleanArray, Column as BatchColumn, DataType as BatchDataType, DictionaryArray,
    Field, Float64Array, Int64Array, RecordBatch, Schema,
};
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
        size_of::<Value>()
            + match self {
                Self::String(values) => values[index].as_ref().map_or(0, String::len),
                _ => 0,
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

    pub(crate) fn owned_value_bytes(&self, row: usize, column: usize) -> usize {
        self.columns[column].owned_value_bytes(row)
    }

    pub(crate) fn record_batch(
        &self,
        start: usize,
        capacity: usize,
        column: usize,
        memory_limit_bytes: usize,
        mut check_cancellation: impl FnMut() -> Result<()>,
    ) -> Result<RecordBatch> {
        debug_assert!(start < self.row_count);
        debug_assert!(capacity > 0);
        debug_assert!(column < self.columns.len());
        check_cancellation()?;
        let end = start.saturating_add(capacity).min(self.row_count);
        let definition = &self.schema[column];
        let data = &self.columns[column];
        let bitmap_bytes = capacity
            .div_ceil(u64::BITS as usize)
            .saturating_mul(size_of::<u64>());
        let common_bytes = size_of::<Field>()
            .saturating_add(definition.name.len())
            .saturating_add(size_of::<BatchColumn>())
            .saturating_add(bitmap_bytes);
        let mut peak_bytes = common_bytes.saturating_add(match data {
            ColumnData::Int64(_) => capacity
                .saturating_mul(size_of::<i64>())
                .saturating_add(bitmap_bytes),
            ColumnData::Float64(_) => capacity
                .saturating_mul(size_of::<f64>())
                .saturating_add(bitmap_bytes),
            ColumnData::Bool(_) => bitmap_bytes.saturating_mul(2),
            ColumnData::String(_) => capacity
                .saturating_mul(size_of::<u32>())
                .saturating_add(capacity.saturating_mul(size_of::<Option<Box<str>>>()))
                .saturating_add(bitmap_bytes)
                .saturating_add(
                    DictionaryArray::build_workspace_bytes(capacity).unwrap_or(usize::MAX),
                ),
        });
        enforce_batch_memory_limit(peak_bytes, memory_limit_bytes)?;
        if let ColumnData::String(values) = data {
            for value in values[start..end].iter().filter_map(Option::as_deref) {
                check_cancellation()?;
                enforce_batch_memory_limit(
                    peak_bytes.saturating_add(value.len()),
                    memory_limit_bytes,
                )?;
            }
        }

        let schema = Schema::new(vec![Field::new(
            definition.name.as_str(),
            match definition.data_type {
                DataType::Int64 => BatchDataType::Int64,
                DataType::Float64 => BatchDataType::Float64,
                DataType::Bool => BatchDataType::Boolean,
                DataType::String => BatchDataType::String,
            },
            definition.nullable,
        )]);
        let batch_column = match data {
            ColumnData::Int64(values) => {
                let mut array = Int64Array::with_capacity(capacity);
                for value in &values[start..end] {
                    check_cancellation()?;
                    array.push(*value)?;
                }
                BatchColumn::Int64(array)
            }
            ColumnData::Float64(values) => {
                let mut array = Float64Array::with_capacity(capacity);
                for value in &values[start..end] {
                    check_cancellation()?;
                    array.push(*value)?;
                }
                BatchColumn::Float64(array)
            }
            ColumnData::Bool(values) => {
                let mut array = BooleanArray::with_capacity(capacity);
                for value in &values[start..end] {
                    check_cancellation()?;
                    array.push(*value)?;
                }
                BatchColumn::Boolean(array)
            }
            ColumnData::String(values) => DictionaryArray::from_options_controlled(
                capacity,
                values[start..end].iter().map(Option::as_deref),
                check_cancellation,
                |string_bytes| {
                    peak_bytes = peak_bytes.saturating_add(string_bytes);
                    enforce_batch_memory_limit(peak_bytes, memory_limit_bytes)
                },
            )
            .map(BatchColumn::String)?,
        };
        RecordBatch::try_new(
            schema,
            vec![batch_column],
            BatchConfig::new(capacity, memory_limit_bytes),
        )
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
        Ok(Self {
            schema,
            columns,
            row_count,
        })
    }
}

fn enforce_batch_memory_limit(required: usize, limit: usize) -> Result<()> {
    if required > limit {
        Err(Error::MemoryLimitExceeded {
            operator: "SELECT batch",
            required,
            limit,
        })
    } else {
        Ok(())
    }
}
