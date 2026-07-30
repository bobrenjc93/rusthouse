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
    name.eq_ignore_ascii_case("TRUE") || name.eq_ignore_ascii_case("FALSE")
}

/// A physical column. Each variant owns a contiguous vector of one Rust type.
#[derive(Debug, Clone)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
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

    fn converted(&self, target: DataType, table_name: &str, column_name: &str) -> Result<Self> {
        let source = self.data_type();
        let failure = |row: Option<usize>, reason: String| Error::ColumnConversion {
            table: table_name.to_owned(),
            column: column_name.to_owned(),
            from: source,
            to: target,
            row,
            reason: reason.into_boxed_str(),
        };

        match (self, target) {
            (Self::Int64(values), DataType::Int64) => Ok(Self::Int64(values.clone())),
            (Self::Int64(values), DataType::Float64) => Ok(Self::Float64(
                values.iter().map(|value| *value as f64).collect(),
            )),
            (Self::Int64(values), DataType::String) => {
                Ok(Self::String(values.iter().map(i64::to_string).collect()))
            }
            (Self::Float64(values), DataType::Int64) => {
                const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
                let mut converted = Vec::with_capacity(values.len());
                for (index, value) in values.iter().copied().enumerate() {
                    let row = Some(index + 1);
                    if !value.is_finite() {
                        return Err(failure(row, format!("non-finite Float64 value {value}")));
                    }
                    if value < i64::MIN as f64 || value >= I64_UPPER_EXCLUSIVE {
                        return Err(failure(row, format!("Int64 overflow for value {value}")));
                    }
                    if value.fract() != 0.0 {
                        return Err(failure(
                            row,
                            format!("Float64 value {value} is not an integer"),
                        ));
                    }
                    converted.push(value as i64);
                }
                Ok(Self::Int64(converted))
            }
            (Self::Float64(values), DataType::Float64) => Ok(Self::Float64(values.clone())),
            (Self::Float64(values), DataType::String) => Ok(Self::String(
                values
                    .iter()
                    .map(|value| Value::Float64(*value).as_display_string())
                    .collect(),
            )),
            (Self::Bool(values), DataType::Bool) => Ok(Self::Bool(values.clone())),
            (Self::Bool(values), DataType::String) => {
                Ok(Self::String(values.iter().map(bool::to_string).collect()))
            }
            (Self::String(values), DataType::Int64) => {
                use std::num::IntErrorKind;

                let mut converted = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    match value.parse::<i64>() {
                        Ok(value) => converted.push(value),
                        Err(error) => {
                            let reason = match error.kind() {
                                IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => {
                                    format!("Int64 overflow parsing {value:?}")
                                }
                                _ => format!("invalid Int64 value {value:?}"),
                            };
                            return Err(failure(Some(index + 1), reason));
                        }
                    }
                }
                Ok(Self::Int64(converted))
            }
            (Self::String(values), DataType::Float64) => {
                let mut converted = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    let parsed = value.parse::<f64>().map_err(|_| {
                        failure(Some(index + 1), format!("invalid Float64 value {value:?}"))
                    })?;
                    if !parsed.is_finite() {
                        return Err(failure(
                            Some(index + 1),
                            format!("non-finite Float64 value parsed from {value:?}"),
                        ));
                    }
                    converted.push(parsed);
                }
                Ok(Self::Float64(converted))
            }
            (Self::String(values), DataType::Bool) => {
                let mut converted = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    if value.eq_ignore_ascii_case("true") {
                        converted.push(true);
                    } else if value.eq_ignore_ascii_case("false") {
                        converted.push(false);
                    } else {
                        return Err(failure(
                            Some(index + 1),
                            format!("invalid Bool value {value:?}; expected true or false"),
                        ));
                    }
                }
                Ok(Self::Bool(converted))
            }
            (Self::String(values), DataType::String) => Ok(Self::String(values.clone())),
            (Self::Int64(_), DataType::Bool)
            | (Self::Float64(_), DataType::Bool)
            | (Self::Bool(_), DataType::Int64 | DataType::Float64) => Err(failure(
                None,
                "this type conversion is not supported".to_owned(),
            )),
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

    /// Validates the complete row before appending one value to each column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.validate_row(&row)?;
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
        Ok(())
    }

    /// Rebuilds a physical column and installs it only after every row converts.
    pub fn modify_column(&mut self, name: &str, data_type: DataType) -> Result<()> {
        let index = self.column_index(name)?;
        let column_name = &self.schema[index].name;
        let replacement = self.columns[index].converted(data_type, &self.name, column_name)?;
        debug_assert_eq!(replacement.len(), self.row_count);

        self.columns[index] = replacement;
        self.schema[index].data_type = data_type;
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
    fn non_finite_float_narrowing_reports_its_row_without_mutation() {
        let mut table = Table {
            name: "readings".to_owned(),
            schema: vec![ColumnDef {
                name: "value".to_owned(),
                data_type: DataType::Float64,
            }],
            columns: vec![Column::Float64(vec![1.0, f64::INFINITY])],
            row_count: 2,
        };

        let error = table
            .modify_column("value", DataType::Int64)
            .expect_err("non-finite value is rejected");
        assert!(matches!(
            error,
            Error::ColumnConversion {
                row: Some(2),
                reason,
                ..
            } if reason.contains("non-finite")
        ));
        assert_eq!(table.schema()[0].data_type, DataType::Float64);
        assert!(matches!(
            &table.columns()[0],
            Column::Float64(values) if values[1].is_infinite()
        ));
    }
}
