use std::collections::HashSet;

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
    Int64(Vec<i64>, Vec<bool>),
    Float64(Vec<f64>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    String(Vec<String>, Vec<bool>),
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new(), Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new(), Vec::new()),
            DataType::Bool => Self::Bool(Vec::new(), Vec::new()),
            DataType::String => Self::String(Vec::new(), Vec::new()),
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_, _) => DataType::Int64,
            Self::Float64(_, _) => DataType::Float64,
            Self::Bool(_, _) => DataType::Bool,
            Self::String(_, _) => DataType::String,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values, _) => values.len(),
            Self::Float64(values, _) => values.len(),
            Self::Bool(values, _) => values.len(),
            Self::String(values, _) => values.len(),
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
            Self::Int64(_, valid)
            | Self::Float64(_, valid)
            | Self::Bool(_, valid)
            | Self::String(_, valid)
                if !valid[row] =>
            {
                ValueRef::Null
            }
            Self::Int64(values, _) => ValueRef::Int64(values[row]),
            Self::Float64(values, _) => ValueRef::Float64(values[row]),
            Self::Bool(values, _) => ValueRef::Bool(values[row]),
            Self::String(values, _) => ValueRef::String(&values[row]),
        }
    }

    pub(crate) fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Int64(_, valid)
            | Self::Float64(_, valid)
            | Self::Bool(_, valid)
            | Self::String(_, valid) => !valid[row],
        }
    }

    pub(crate) fn cmp_at(&self, left: usize, right: usize) -> std::cmp::Ordering {
        self.value_ref(left).cmp(&self.value_ref(right))
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values, valid), Value::Int64(value)) => {
                values.push(value);
                valid.push(true);
            }
            (Self::Float64(values, valid), Value::Float64(value)) => {
                values.push(value);
                valid.push(true);
            }
            (Self::Bool(values, valid), Value::Bool(value)) => {
                values.push(value);
                valid.push(true);
            }
            (Self::String(values, valid), Value::String(value)) => {
                values.push(value);
                valid.push(true);
            }
            (Self::Int64(values, valid), Value::Null) => {
                values.push(0);
                valid.push(false);
            }
            (Self::Float64(values, valid), Value::Null) => {
                values.push(0.0);
                valid.push(false);
            }
            (Self::Bool(values, valid), Value::Null) => {
                values.push(false);
                valid.push(false);
            }
            (Self::String(values, valid), Value::Null) => {
                values.push(String::new());
                valid.push(false);
            }
            _ => unreachable!("values are validated before insertion"),
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
            if value
                .data_type()
                .is_some_and(|data_type| field.data_type != data_type)
            {
                return Err(Error::TypeMismatch {
                    context: format!("column '{}.{}'", self.name, field.name),
                    expected: field.data_type.to_string(),
                    actual: value
                        .data_type()
                        .expect("non-NULL type mismatch")
                        .to_string(),
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

        assert!(
            matches!(&table.columns()[0], Column::Int64(v, valid) if v == &[7] && valid == &[true])
        );
        assert!(
            matches!(&table.columns()[1], Column::String(v, valid) if v == &["ok"] && valid == &[true])
        );
    }

    #[test]
    fn stores_nulls_in_column_validity_bitmaps() {
        let mut table = test_table();
        table
            .insert_row(vec![Value::Null, Value::Null])
            .expect("NULL is valid for every column type");

        assert!(
            matches!(&table.columns()[0], Column::Int64(v, valid) if v == &[0] && valid == &[false])
        );
        assert!(
            matches!(&table.columns()[1], Column::String(v, valid) if v == &[""] && valid == &[false])
        );
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
