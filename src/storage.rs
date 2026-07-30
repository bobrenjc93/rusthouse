//! Typed, contiguous column storage and table-level row validation.

use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::value::{DataType, Value, ValueRef};

/// A named, typed field in a table schema.
///
/// A table preserves definitions in caller-supplied order. Names are matched
/// case-insensitively, while the original spelling remains available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// Original, case-preserving column name.
    pub name: String,
    /// Exact physical type required for inserted values.
    pub data_type: DataType,
}

pub(crate) fn is_reserved_column_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("TRUE") || name.eq_ignore_ascii_case("FALSE")
}

/// A physical column that owns a contiguous vector of one Rust type.
///
/// Vector index is row insertion order. The variant fixes the physical type
/// for the column's lifetime; all contained values are owned and remain valid
/// until removed by consuming or directly mutating the public vector.
#[derive(Debug, Clone)]
pub enum Column {
    /// Contiguous signed 64-bit integers.
    Int64(Vec<i64>),
    /// Contiguous double-precision floating-point numbers.
    Float64(Vec<f64>),
    /// Contiguous Boolean values.
    Bool(Vec<bool>),
    /// Contiguous owned UTF-8 strings.
    String(Vec<String>),
}

impl Column {
    /// Creates an empty column with the variant selected by `data_type`.
    ///
    /// This operation is infallible and preserves the requested physical type
    /// for the lifetime of the returned column.
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    /// Returns the physical type represented by this column's variant.
    ///
    /// This operation is infallible, performs no allocation or mutation, and
    /// returns an owned type marker.
    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of stored row values.
    ///
    /// This is an infallible, constant-time query that does not borrow any
    /// individual value.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    /// Returns whether the column contains no row values.
    ///
    /// This is equivalent to `self.len() == 0` and does not mutate the column.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clones the value at zero-based row index `row` into an owned [`Value`].
    ///
    /// The returned value is independent of the column and may outlive it.
    /// Row indices follow insertion order.
    ///
    /// # Panics
    ///
    /// Panics when `row >= self.len()`.
    #[must_use]
    pub fn value(&self, row: usize) -> Value {
        self.value_ref(row).to_owned()
    }

    pub(crate) fn value_ref(&self, row: usize) -> ValueRef<'_> {
        match self {
            Self::Int64(values) => ValueRef::Int64(values[row]),
            Self::Float64(values) => ValueRef::Float64(values[row]),
            Self::Bool(values) => ValueRef::Bool(values[row]),
            Self::String(values) => ValueRef::String(&values[row]),
        }
    }

    pub(crate) fn cmp_at(&self, left: usize, right: usize) -> std::cmp::Ordering {
        self.value_ref(left).cmp(&self.value_ref(right))
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("values are validated before insertion"),
        }
    }
}

/// A table that stores one typed vector per schema field.
///
/// Schema and column order are identical and stable. Successful inserts append
/// at the next row index, so rows remain in insertion order for the table's
/// lifetime. A table owns its schema and data; borrows returned by its
/// accessors cannot outlive it.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    /// Creates an empty table and one physical column per schema field.
    ///
    /// The supplied table name is retained verbatim. Schema order is retained,
    /// while column uniqueness and reserved-name checks ignore ASCII case.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidQuery`] for an empty schema,
    /// [`Error::ReservedIdentifier`] for columns named `TRUE` or `FALSE`, or
    /// [`Error::DuplicateColumn`] for a repeated case-insensitive name. Since
    /// construction has no external side effects, every error is atomic.
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

    /// Borrows the original, case-preserving table name.
    ///
    /// This operation is infallible and the returned string remains valid for
    /// the shared borrow of `self`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrows schema fields in physical column order.
    ///
    /// This operation is infallible. The slice and its owned field names remain
    /// valid only for the shared borrow of `self`.
    #[must_use]
    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    /// Borrows physical columns in schema order.
    ///
    /// Every column has [`Table::row_count`] values when the table has only
    /// been changed through [`Table::insert_row`]. The slice remains valid only
    /// for the shared borrow of `self`.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns the number of successfully inserted rows.
    ///
    /// Row indices range from zero to this value, exclusive, in insertion
    /// order. This operation is infallible and does not mutate the table.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the physical index of the case-insensitively named column.
    ///
    /// The index follows schema order and remains stable for the table's
    /// lifetime because schemas cannot be changed after construction.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ColumnNotFound`] without mutation when no field
    /// matches `name`.
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
            if field.data_type != value.data_type() {
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

    /// Validates and appends one complete row.
    ///
    /// Values must match the schema width and exact physical types; non-finite
    /// `Float64` values are rejected. On success, one value is appended to each
    /// column at the same new row index.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RowLength`], [`Error::TypeMismatch`], or
    /// [`Error::InvalidQuery`] as appropriate. Validation completes before any
    /// column is changed, so the table is unchanged on every returned error.
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
