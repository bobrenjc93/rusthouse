use std::collections::HashSet;
use std::mem;

use crate::error::{Error, Result};
use crate::value::{DataType, Value, ValueRef};

/// A named, typed field in a table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
}

pub(crate) fn is_reserved_column_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("TRUE")
        || name.eq_ignore_ascii_case("FALSE")
        || name.eq_ignore_ascii_case("NULL")
}

/// A physical column. Each variant owns a contiguous vector of one Rust type.
#[derive(Debug, Clone)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
    Null(Vec<()>),
    #[doc(hidden)]
    Nullable(NullableColumn),
}

/// Internal nullable-column storage exposed only to make [`Column`] a public enum.
/// Its fields are private so callers cannot create an invalid validity bitmap.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct NullableColumn {
    values: Box<Column>,
    valid: Vec<bool>,
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Null => Self::Null(Vec::new()),
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Null(_) => DataType::Null,
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
            Self::Nullable(nullable) => nullable.values.data_type(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Null(values) => values.len(),
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
            Self::Nullable(nullable) => {
                debug_assert_eq!(nullable.values.len(), nullable.valid.len());
                nullable.values.len()
            }
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn value(&self, row: usize) -> Value {
        self.value_ref(row).to_owned()
    }

    pub(crate) fn value_ref(&self, row: usize) -> ValueRef<'_> {
        match self {
            Self::Null(values) => {
                let () = values[row];
                ValueRef::Null
            }
            Self::Int64(values) => ValueRef::Int64(values[row]),
            Self::Float64(values) => ValueRef::Float64(values[row]),
            Self::Bool(values) => ValueRef::Bool(values[row]),
            Self::String(values) => ValueRef::String(&values[row]),
            Self::Nullable(nullable) if !nullable.valid[row] => ValueRef::Null,
            Self::Nullable(nullable) => nullable.values.value_ref(row),
        }
    }

    pub(crate) fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Null(values) => {
                let () = values[row];
                true
            }
            Self::Nullable(nullable) => !nullable.valid[row],
            Self::Int64(values) => {
                let _ = values[row];
                false
            }
            Self::Float64(values) => {
                let _ = values[row];
                false
            }
            Self::Bool(values) => {
                let _ = values[row];
                false
            }
            Self::String(values) => {
                let _ = &values[row];
                false
            }
        }
    }

    pub(crate) fn cmp_at(&self, left: usize, right: usize) -> std::cmp::Ordering {
        self.value_ref(left).cmp(&self.value_ref(right))
    }

    pub(crate) fn int64_values(&self) -> Option<&[i64]> {
        match self {
            Self::Int64(values) => Some(values),
            Self::Nullable(nullable) => nullable.values.int64_values(),
            _ => None,
        }
    }

    pub(crate) fn float64_values(&self) -> Option<&[f64]> {
        match self {
            Self::Float64(values) => Some(values),
            Self::Nullable(nullable) => nullable.values.float64_values(),
            _ => None,
        }
    }

    fn push(&mut self, value: Value) {
        if value == Value::Null {
            self.push_null();
            return;
        }

        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            (Self::Nullable(nullable), value) => {
                nullable.values.push(value);
                nullable.valid.push(true);
            }
            _ => unreachable!("values are validated before insertion"),
        }
    }

    fn push_null(&mut self) {
        match self {
            Self::Null(values) => values.push(()),
            Self::Nullable(nullable) => {
                nullable.values.push_placeholder();
                nullable.valid.push(false);
            }
            _ => {
                let data_type = self.data_type();
                let row_count = self.len();
                let values = mem::replace(self, Self::new(data_type));
                let mut nullable = NullableColumn {
                    values: Box::new(values),
                    valid: vec![true; row_count],
                };
                nullable.values.push_placeholder();
                nullable.valid.push(false);
                *self = Self::Nullable(nullable);
            }
        }
    }

    fn push_placeholder(&mut self) {
        match self {
            Self::Null(values) => values.push(()),
            Self::Int64(values) => values.push(0),
            Self::Float64(values) => values.push(0.0),
            Self::Bool(values) => values.push(false),
            Self::String(values) => values.push(String::new()),
            Self::Nullable(_) => unreachable!("nullable columns cannot be nested"),
        }
    }
}

/// A table stores one typed vector per schema field.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    pub fn new(name: String, schema: Vec<ColumnDef>) -> Result<Self> {
        if schema.is_empty() {
            return Err(Error::InvalidQuery(
                "a table must contain at least one column".to_owned(),
            ));
        }
        let mut column_names = HashSet::with_capacity(schema.len());
        for field in &schema {
            if is_reserved_column_name(&field.name) {
                return Err(Error::ReservedIdentifier {
                    identifier: field.name.clone(),
                    context: "column name".to_owned(),
                });
            }
            if !column_names.insert(field.name.to_ascii_lowercase()) {
                return Err(Error::DuplicateColumn(field.name.clone()));
            }
        }
        let columns = schema
            .iter()
            .map(|field| Column::new(field.data_type))
            .collect();
        Ok(Self {
            name,
            schema,
            columns,
            row_count: 0,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub fn column_index(&self, name: &str) -> Result<usize> {
        self.schema
            .iter()
            .position(|field| field.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::ColumnNotFound {
                table: self.name.clone(),
                column: name.to_owned(),
            })
    }

    /// Checks a row without mutating any physical column.
    pub(crate) fn validate_row(&self, row: &[Value]) -> Result<()> {
        if row.len() != self.schema.len() {
            return Err(Error::RowLength {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (field, value) in self.schema.iter().zip(row) {
            if !matches!(value, Value::Null) && field.data_type != value.data_type() {
                return Err(Error::TypeMismatch {
                    context: format!("column '{}.{}'", self.name, field.name),
                    expected: field.data_type.to_string(),
                    actual: value.data_type().to_string(),
                });
            }
            if matches!(value, Value::Float64(number) if !number.is_finite()) {
                return Err(Error::InvalidQuery(format!(
                    "column '{}.{}' cannot store a non-finite Float64",
                    self.name, field.name
                )));
            }
        }

        Ok(())
    }

    /// Validates the complete row before appending one value to each column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.validate_row(&row)?;
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_table() -> Table {
        Table::new(
            "events".to_owned(),
            vec![
                ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
        )
        .expect("valid schema")
    }

    #[test]
    fn stores_values_in_typed_columns() {
        let mut table = test_table();
        table
            .insert_row(vec![Value::Int64(7), Value::String("ok".to_owned())])
            .expect("valid row");

        assert!(matches!(&table.columns()[0], Column::Int64(v) if v == &[7]));
        assert!(matches!(&table.columns()[1], Column::String(v) if v == &["ok"]));
    }

    #[test]
    fn stores_nulls_in_column_validity_bitmaps() {
        let mut table = test_table();
        table
            .insert_row(vec![Value::Null, Value::Null])
            .expect("NULL is valid for every column type");

        assert!(matches!(
            &table.columns()[0],
            Column::Nullable(nullable)
                if nullable.values.len() == 1 && nullable.valid == [false]
        ));
        assert!(matches!(
            &table.columns()[1],
            Column::Nullable(nullable)
                if nullable.values.len() == 1 && nullable.valid == [false]
        ));
        assert_eq!(table.columns()[0].value(0), Value::Null);
        assert_eq!(table.columns()[1].value(0), Value::Null);
    }

    #[test]
    fn rejected_rows_do_not_partially_mutate_columns() {
        let mut table = test_table();
        let error = table
            .insert_row(vec![Value::Int64(7), Value::Bool(true)])
            .expect_err("wrong type");

        assert!(matches!(error, Error::TypeMismatch { .. }));
        assert_eq!(table.row_count(), 0);
        assert!(table.columns().iter().all(Column::is_empty));
    }
}
