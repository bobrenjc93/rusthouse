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

const VALIDITY_WORD_BITS: usize = u64::BITS as usize;

/// A packed nullable-column validity bitmap using one bit per row.
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

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn get(&self, row: usize) -> Option<bool> {
        (row < self.len).then(|| {
            let word = self.words[row / VALIDITY_WORD_BITS];
            word & (1_u64 << (row % VALIDITY_WORD_BITS)) != 0
        })
    }

    /// Returns the packed words, least-significant bit first within each word.
    #[must_use]
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    fn push(&mut self, valid: bool) {
        let bit = self.len % VALIDITY_WORD_BITS;
        if bit == 0 {
            self.words.push(0);
        }
        if valid {
            let word = self.words.last_mut().expect("a word was just allocated");
            *word |= 1_u64 << bit;
        }
        self.len += 1;
    }
}

/// A physical column. Nullable columns pair typed values with a packed validity bitmap.
#[derive(Debug, Clone)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
    Nullable {
        values: Box<Self>,
        validity: ValidityBitmap,
    },
}

impl Column {
    pub fn new(data_type: DataType) -> Result<Self> {
        match data_type {
            DataType::Int64 => Ok(Self::Int64(Vec::new())),
            DataType::Float64 => Ok(Self::Float64(Vec::new())),
            DataType::Bool => Ok(Self::Bool(Vec::new())),
            DataType::String => Ok(Self::String(Vec::new())),
            data_type @ (DataType::NullableInt64
            | DataType::NullableFloat64
            | DataType::NullableBool
            | DataType::NullableString) => Ok(Self::Nullable {
                values: Box::new(Self::new(data_type.underlying())?),
                validity: ValidityBitmap::new(),
            }),
            DataType::Null => Err(Error::InvalidQuery(
                "NULL is a literal type, not a physical column type; use Nullable(T)".to_owned(),
            )),
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
            Self::Nullable { values, .. } => values.data_type().nullable(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
            Self::Nullable { validity, .. } => validity.len(),
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

    /// Returns the packed validity bitmap for a nullable column.
    #[must_use]
    pub fn validity(&self) -> Option<&ValidityBitmap> {
        match self {
            Self::Nullable { validity, .. } => Some(validity),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_null(&self, row: usize) -> bool {
        matches!(self, Self::Nullable { validity, .. } if !validity.get(row).expect("row is in bounds"))
    }

    pub(crate) fn value_ref(&self, row: usize) -> ValueRef<'_> {
        match self {
            Self::Int64(values) => ValueRef::Int64(values[row]),
            Self::Float64(values) => ValueRef::Float64(values[row]),
            Self::Bool(values) => ValueRef::Bool(values[row]),
            Self::String(values) => ValueRef::String(&values[row]),
            Self::Nullable { values, validity } => {
                if validity.get(row).expect("row is in bounds") {
                    values.value_ref(row)
                } else {
                    ValueRef::Null
                }
            }
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
            (Self::Nullable { values, validity }, Value::Null) => {
                values.push_default();
                validity.push(false);
            }
            (Self::Nullable { values, validity }, value) => {
                values.push(value);
                validity.push(true);
            }
            _ => unreachable!("values are validated before insertion"),
        }
    }

    fn push_default(&mut self) {
        match self {
            Self::Int64(values) => values.push(0),
            Self::Float64(values) => values.push(0.0),
            Self::Bool(values) => values.push(false),
            Self::String(values) => values.push(String::new()),
            Self::Nullable { .. } => unreachable!("nullable columns cannot be nested"),
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
            if field.data_type == DataType::Null {
                return Err(Error::InvalidQuery(format!(
                    "column '{}.{}' cannot use NULL as a data type; use Nullable(T)",
                    name, field.name
                )));
            }
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
            .collect::<Result<Vec<_>>>()?;
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
            if !field.data_type.accepts(value.data_type()) {
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
    fn nullable_columns_pack_validity_across_word_boundaries() {
        let mut table = Table::new(
            "samples".to_owned(),
            vec![ColumnDef {
                name: "value".to_owned(),
                data_type: DataType::NullableInt64,
            }],
        )
        .expect("valid nullable schema");

        let valid_rows = [0, 63, 64, 65, 127, 129];
        for row in 0..130 {
            let value = if valid_rows.contains(&row) {
                Value::Int64(row as i64)
            } else {
                Value::Null
            };
            table.insert_row(vec![value]).expect("valid row");
        }

        let Column::Nullable { values, validity } = &table.columns()[0] else {
            panic!("expected nullable physical storage");
        };
        assert!(matches!(values.as_ref(), Column::Int64(values) if values.len() == 130));
        assert_eq!(validity.len(), 130);
        assert_eq!(
            validity.words(),
            &[
                1 | (1_u64 << 63),
                1 | (1_u64 << 1) | (1_u64 << 63),
                1_u64 << 1,
            ]
        );
        assert_eq!(validity.get(130), None);
        for row in valid_rows {
            assert_eq!(table.columns()[0].value(row), Value::Int64(row as i64));
        }
        for row in [1, 62, 66, 126, 128] {
            assert_eq!(table.columns()[0].value(row), Value::Null);
        }
    }
}
