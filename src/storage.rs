use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::types::{DataType, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Result<Self> {
        if fields.is_empty() {
            return Err(Error::Constraint(
                "a table must contain at least one column".into(),
            ));
        }
        let mut names = HashSet::new();
        for field in &fields {
            let normalized = normalize_identifier(&field.name);
            if !names.insert(normalized) {
                return Err(Error::DuplicateColumn(field.name.clone()));
            }
        }
        Ok(Self { fields })
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn index_of(&self, name: &str) -> Result<usize> {
        let normalized = normalize_identifier(name);
        self.fields
            .iter()
            .position(|field| normalize_identifier(&field.name) == normalized)
            .ok_or_else(|| Error::ColumnNotFound(name.into()))
    }
}

#[derive(Debug, Clone)]
pub enum ColumnVector {
    Int64(Vec<Option<i64>>),
    Float64(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    String(Vec<Option<String>>),
}

impl ColumnVector {
    fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, row: usize) -> Value {
        match self {
            Self::Int64(values) => values[row].map_or(Value::Null, Value::Int64),
            Self::Float64(values) => values[row].map_or(Value::Null, Value::Float64),
            Self::Bool(values) => values[row].map_or(Value::Null, Value::Bool),
            Self::String(values) => values[row]
                .as_ref()
                .map_or(Value::Null, |value| Value::String(value.clone())),
        }
    }

    fn value_retained_bytes(&self, row: usize) -> usize {
        std::mem::size_of::<Value>().saturating_add(match self {
            Self::String(values) => values[row].as_ref().map_or(0, String::len),
            _ => 0,
        })
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(Some(value)),
            (Self::Int64(values), Value::Null) => values.push(None),
            (Self::Float64(values), Value::Float64(value)) => values.push(Some(value)),
            (Self::Float64(values), Value::Null) => values.push(None),
            (Self::Bool(values), Value::Bool(value)) => values.push(Some(value)),
            (Self::Bool(values), Value::Null) => values.push(None),
            (Self::String(values), Value::String(value)) => values.push(Some(value)),
            (Self::String(values), Value::Null) => values.push(None),
            _ => unreachable!("values are validated before they reach a column"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Table {
    schema: Schema,
    columns: Vec<ColumnVector>,
}

impl Table {
    pub fn new(schema: Schema) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|field| ColumnVector::new(field.data_type))
            .collect();
        Self { schema, columns }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn row_count(&self) -> usize {
        self.columns.first().map_or(0, ColumnVector::len)
    }

    pub fn value(&self, row: usize, column: usize) -> Value {
        self.columns[column].get(row)
    }

    pub(crate) fn value_retained_bytes(&self, row: usize, column: usize) -> usize {
        self.columns[column].value_retained_bytes(row)
    }

    /// Validates the entire batch before mutating any column.
    pub fn append_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        let mut converted = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() != self.schema.len() {
                return Err(Error::Constraint(format!(
                    "expected {} values, found {}",
                    self.schema.len(),
                    row.len()
                )));
            }
            let row = row
                .into_iter()
                .zip(self.schema.fields())
                .map(|(value, field)| coerce_for_field(value, field))
                .collect::<Result<Vec<_>>>()?;
            converted.push(row);
        }

        for row in converted {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        Ok(())
    }
}

fn coerce_for_field(value: Value, field: &Field) -> Result<Value> {
    match (value, field.data_type) {
        (Value::Null, _) if field.nullable => Ok(Value::Null),
        (Value::Null, _) => Err(Error::Constraint(format!(
            "column {} is not nullable",
            field.name
        ))),
        (value @ Value::Int64(_), DataType::Int64)
        | (value @ Value::Float64(_), DataType::Float64)
        | (value @ Value::Bool(_), DataType::Bool)
        | (value @ Value::String(_), DataType::String) => Ok(value),
        (Value::Int64(value), DataType::Float64) => Ok(Value::Float64(value as f64)),
        (value, expected) => Err(Error::Type(format!(
            "column {} expects {}, found {}",
            field.name,
            expected,
            value.type_name()
        ))),
    }
}

pub(crate) fn normalize_identifier(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_insert_is_atomic() {
        let schema = Schema::new(vec![Field {
            name: "n".into(),
            data_type: DataType::Int64,
            nullable: false,
        }])
        .unwrap();
        let mut table = Table::new(schema);
        let error = table
            .append_rows(vec![vec![Value::Int64(1)], vec![Value::String("x".into())]])
            .unwrap_err();
        assert!(matches!(error, Error::Type(_)));
        assert_eq!(table.row_count(), 0);
    }
}
