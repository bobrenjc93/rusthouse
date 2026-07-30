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

/// A packed bitmap recording which rows in a nullable column contain a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidityBitmap {
    words: Vec<u64>,
    len: usize,
}

impl ValidityBitmap {
    fn new() -> Self {
        Self {
            words: Vec::new(),
            len: 0,
        }
    }

    fn push(&mut self, valid: bool) {
        let bit = self.len % u64::BITS as usize;
        if bit == 0 {
            self.words.push(0);
        }
        if valid {
            let word = self.words.last_mut().expect("a word was just allocated");
            *word |= 1_u64 << bit;
        }
        self.len += 1;
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn is_valid(&self, row: usize) -> bool {
        assert!(row < self.len, "validity bitmap row is out of bounds");
        self.words[row / u64::BITS as usize] & (1_u64 << (row % u64::BITS as usize)) != 0
    }
}

/// A physical column with a contiguous typed payload and optional validity bitmap.
#[derive(Debug, Clone)]
pub enum Column {
    Int64 {
        values: Vec<i64>,
        validity: Option<ValidityBitmap>,
    },
    Float64 {
        values: Vec<f64>,
        validity: Option<ValidityBitmap>,
    },
    Bool {
        values: Vec<bool>,
        validity: Option<ValidityBitmap>,
    },
    String {
        values: Vec<String>,
        validity: Option<ValidityBitmap>,
    },
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        let validity = data_type.is_nullable().then(ValidityBitmap::new);
        match data_type.underlying_type() {
            DataType::Int64 => Self::Int64 {
                values: Vec::new(),
                validity,
            },
            DataType::Float64 => Self::Float64 {
                values: Vec::new(),
                validity,
            },
            DataType::Bool => Self::Bool {
                values: Vec::new(),
                validity,
            },
            DataType::String => Self::String {
                values: Vec::new(),
                validity,
            },
            DataType::NullableInt64
            | DataType::NullableFloat64
            | DataType::NullableBool
            | DataType::NullableString => {
                unreachable!("underlying_type returns a physical type")
            }
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        let (data_type, nullable) = match self {
            Self::Int64 { validity, .. } => (DataType::Int64, validity.is_some()),
            Self::Float64 { validity, .. } => (DataType::Float64, validity.is_some()),
            Self::Bool { validity, .. } => (DataType::Bool, validity.is_some()),
            Self::String { validity, .. } => (DataType::String, validity.is_some()),
        };
        if nullable {
            DataType::nullable(data_type)
        } else {
            data_type
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64 { values, .. } => values.len(),
            Self::Float64 { values, .. } => values.len(),
            Self::Bool { values, .. } => values.len(),
            Self::String { values, .. } => values.len(),
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

    #[must_use]
    pub fn validity(&self) -> Option<&ValidityBitmap> {
        match self {
            Self::Int64 { validity, .. }
            | Self::Float64 { validity, .. }
            | Self::Bool { validity, .. }
            | Self::String { validity, .. } => validity.as_ref(),
        }
    }

    pub(crate) fn is_valid(&self, row: usize) -> bool {
        self.validity()
            .is_none_or(|validity| validity.is_valid(row))
    }

    pub(crate) fn value_ref(&self, row: usize) -> ValueRef<'_> {
        if !self.is_valid(row) {
            return ValueRef::Null;
        }
        match self {
            Self::Int64 { values, .. } => ValueRef::Int64(values[row]),
            Self::Float64 { values, .. } => ValueRef::Float64(values[row]),
            Self::Bool { values, .. } => ValueRef::Bool(values[row]),
            Self::String { values, .. } => ValueRef::String(&values[row]),
        }
    }

    pub(crate) fn cmp_at(&self, left: usize, right: usize) -> std::cmp::Ordering {
        self.value_ref(left).cmp(&self.value_ref(right))
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64 { values, validity }, Value::Int64(value)) => {
                values.push(value);
                push_validity(validity, true);
            }
            (Self::Float64 { values, validity }, Value::Float64(value)) => {
                values.push(value);
                push_validity(validity, true);
            }
            (Self::Bool { values, validity }, Value::Bool(value)) => {
                values.push(value);
                push_validity(validity, true);
            }
            (Self::String { values, validity }, Value::String(value)) => {
                values.push(value);
                push_validity(validity, true);
            }
            (Self::Int64 { values, validity }, Value::Null) => {
                values.push(0);
                push_null(validity);
            }
            (Self::Float64 { values, validity }, Value::Null) => {
                values.push(0.0);
                push_null(validity);
            }
            (Self::Bool { values, validity }, Value::Null) => {
                values.push(false);
                push_null(validity);
            }
            (Self::String { values, validity }, Value::Null) => {
                values.push(String::new());
                push_null(validity);
            }
            _ => unreachable!("values are validated before insertion"),
        }
    }
}

fn push_validity(validity: &mut Option<ValidityBitmap>, valid: bool) {
    if let Some(validity) = validity {
        validity.push(valid);
    }
}

fn push_null(validity: &mut Option<ValidityBitmap>) {
    validity
        .as_mut()
        .expect("NULL is only valid in nullable columns")
        .push(false);
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
            let valid_type = match value.data_type() {
                None => field.data_type.is_nullable(),
                Some(value_type) => field.data_type.underlying_type() == value_type,
            };
            if !valid_type {
                return Err(Error::TypeMismatch {
                    context: format!("column '{}.{}'", self.name, field.name),
                    expected: field.data_type.to_string(),
                    actual: value.type_name(),
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

        assert!(matches!(
            &table.columns()[0],
            Column::Int64 { values, validity: None } if values == &[7]
        ));
        assert!(matches!(
            &table.columns()[1],
            Column::String { values, validity: None } if values == &["ok"]
        ));
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

    #[test]
    fn validity_bitmap_spans_machine_words() {
        let mut table = Table::new(
            "nullable_values".to_owned(),
            vec![ColumnDef {
                name: "value".to_owned(),
                data_type: DataType::nullable(DataType::Int64),
            }],
        )
        .expect("valid schema");

        for row in 0..130 {
            let value = if row % 3 == 0 {
                Value::Null
            } else {
                Value::Int64(row)
            };
            table.insert_row(vec![value]).expect("valid nullable row");
        }

        let validity = table.columns()[0].validity().expect("validity bitmap");
        assert_eq!(validity.len(), 130);
        for row in 0..130 {
            assert_eq!(validity.is_valid(row), row % 3 != 0);
        }
    }
}
