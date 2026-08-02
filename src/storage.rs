use std::collections::HashSet;

use crate::{DataType, Error, Result, Value};

/// A named, typed field in a [`Table`] schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// The column name.
    pub name: String,
    /// The physical type stored by the column.
    pub data_type: DataType,
}

impl ColumnDef {
    /// Creates a column definition.
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }
}

/// A physical column backed by a contiguous vector of one Rust type.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// A column backed by `Vec<i64>`.
    Int64(Vec<i64>),
    /// A column backed by `Vec<f64>`.
    Float64(Vec<f64>),
    /// A column backed by `Vec<bool>`.
    Bool(Vec<bool>),
    /// A column backed by `Vec<String>`.
    String(Vec<String>),
}

impl Column {
    /// Creates an empty physical column for `data_type`.
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    /// Returns the physical type of this column.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of values stored in the column.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    /// Returns `true` when the column contains no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the value at `row`, or `None` when the row is out of bounds.
    ///
    /// String values are cloned because [`Value`] owns its contents.
    #[must_use]
    pub fn value(&self, row: usize) -> Option<Value> {
        match self {
            Self::Int64(values) => values.get(row).copied().map(Value::Int64),
            Self::Float64(values) => values.get(row).copied().map(Value::Float64),
            Self::Bool(values) => values.get(row).copied().map(Value::Bool),
            Self::String(values) => values.get(row).cloned().map(Value::String),
        }
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("rows are validated before insertion"),
        }
    }
}

/// An in-memory table that stores one typed [`Column`] per schema field.
///
/// The schema and physical columns remain in the same order. All insertion
/// validation is completed before any column is changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    /// Creates an empty table with a validated schema.
    ///
    /// Returns [`Error::EmptySchema`] for a schema without fields and
    /// [`Error::DuplicateColumn`] when names repeat, ignoring ASCII case.
    pub fn new(name: impl Into<String>, schema: Vec<ColumnDef>) -> Result<Self> {
        if schema.is_empty() {
            return Err(Error::EmptySchema);
        }

        let mut normalized_names = HashSet::with_capacity(schema.len());
        for field in &schema {
            if !normalized_names.insert(field.name.to_ascii_lowercase()) {
                return Err(Error::DuplicateColumn(field.name.clone()));
            }
        }

        let columns = schema
            .iter()
            .map(|field| Column::new(field.data_type))
            .collect();

        Ok(Self {
            name: name.into(),
            schema,
            columns,
            row_count: 0,
        })
    }

    /// Returns the table name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the ordered schema.
    #[must_use]
    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    /// Returns the ordered physical columns.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns a physical column by position, or `None` when out of bounds.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    /// Returns the number of rows in the table.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns `true` when the table contains no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Finds a column position by its ASCII case-insensitive name.
    pub fn column_index(&self, name: &str) -> Result<usize> {
        self.schema
            .iter()
            .position(|field| field.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::ColumnNotFound {
                table: self.name.clone(),
                column: name.to_owned(),
            })
    }

    /// Returns a value by row and column position, or `None` when either is
    /// out of bounds.
    #[must_use]
    pub fn value(&self, row: usize, column: usize) -> Option<Value> {
        self.column(column)?.value(row)
    }

    /// Validates a row without changing the table.
    pub fn validate_row(&self, row: &[Value]) -> Result<()> {
        if row.len() != self.schema.len() {
            return Err(Error::RowLength {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (field, value) in self.schema.iter().zip(row) {
            let actual = value.data_type();
            if field.data_type != actual {
                return Err(Error::TypeMismatch {
                    table: self.name.clone(),
                    column: field.name.clone(),
                    expected: field.data_type,
                    actual,
                });
            }
            if matches!(value, Value::Float64(number) if !number.is_finite()) {
                return Err(Error::NonFiniteFloat {
                    table: self.name.clone(),
                    column: field.name.clone(),
                });
            }
        }

        Ok(())
    }

    /// Validates and appends one complete row to the table.
    ///
    /// No column is changed when validation returns an error.
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

    fn four_type_table() -> Table {
        Table::new(
            "events",
            vec![
                ColumnDef::new("id", DataType::Int64),
                ColumnDef::new("score", DataType::Float64),
                ColumnDef::new("enabled", DataType::Bool),
                ColumnDef::new("label", DataType::String),
            ],
        )
        .expect("valid schema")
    }

    #[test]
    fn stores_every_type_in_its_physical_vector() {
        let mut table = four_type_table();
        table
            .insert_row(vec![
                Value::Int64(7),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("ready".to_owned()),
            ])
            .expect("valid row");

        assert!(matches!(&table.columns()[0], Column::Int64(v) if v == &[7]));
        assert!(matches!(&table.columns()[1], Column::Float64(v) if v == &[2.5]));
        assert!(matches!(&table.columns()[2], Column::Bool(v) if v == &[true]));
        assert!(matches!(&table.columns()[3], Column::String(v) if v == &["ready"]));
        assert_eq!(table.row_count(), 1);
    }

    #[test]
    fn access_is_bounds_checked() {
        let mut table = four_type_table();
        table
            .insert_row(vec![
                Value::Int64(7),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("ready".to_owned()),
            ])
            .expect("valid row");

        assert_eq!(table.value(0, 0), Some(Value::Int64(7)));
        assert_eq!(table.value(0, 3), Some(Value::String("ready".to_owned())));
        assert_eq!(table.value(1, 0), None);
        assert_eq!(table.value(0, 4), None);
        assert_eq!(table.column(4), None);
        assert_eq!(table.column_index("SCORE"), Ok(1));
        assert!(matches!(
            table.column_index("missing"),
            Err(Error::ColumnNotFound { .. })
        ));
    }

    #[test]
    fn rejects_empty_and_case_insensitively_duplicated_schemas() {
        assert_eq!(Table::new("empty", vec![]), Err(Error::EmptySchema));

        let duplicate = Table::new(
            "duplicate",
            vec![
                ColumnDef::new("EventId", DataType::Int64),
                ColumnDef::new("eventid", DataType::String),
            ],
        );
        assert_eq!(duplicate, Err(Error::DuplicateColumn("eventid".to_owned())));
    }

    #[test]
    fn validates_wide_schemas_without_pairwise_name_scans() {
        let schema: Vec<_> = (0..20_000)
            .map(|index| {
                ColumnDef::new(
                    format!("event_attribute_with_a_long_shared_prefix_{index:05}"),
                    DataType::Int64,
                )
            })
            .collect();
        let mut duplicate_schema = schema.clone();
        duplicate_schema.last_mut().expect("non-empty schema").name =
            schema[0].name.to_ascii_uppercase();

        let table = Table::new("wide", schema).expect("unique wide schema");
        assert_eq!(table.schema().len(), 20_000);
        assert_eq!(
            Table::new("duplicate", duplicate_schema),
            Err(Error::DuplicateColumn(
                "EVENT_ATTRIBUTE_WITH_A_LONG_SHARED_PREFIX_00000".to_owned()
            ))
        );
    }

    #[test]
    fn every_rejected_row_is_atomic() {
        let mut table = four_type_table();
        let original = table.clone();

        let invalid_rows = [
            (
                vec![Value::Int64(1)],
                Error::RowLength {
                    table: "events".to_owned(),
                    expected: 4,
                    actual: 1,
                },
            ),
            (
                vec![
                    Value::Int64(1),
                    Value::Float64(1.0),
                    Value::Bool(false),
                    Value::String("too many".to_owned()),
                    Value::Int64(5),
                ],
                Error::RowLength {
                    table: "events".to_owned(),
                    expected: 4,
                    actual: 5,
                },
            ),
            (
                vec![
                    Value::Int64(1),
                    Value::Float64(1.0),
                    Value::Bool(false),
                    Value::Bool(true),
                ],
                Error::TypeMismatch {
                    table: "events".to_owned(),
                    column: "label".to_owned(),
                    expected: DataType::String,
                    actual: DataType::Bool,
                },
            ),
            (
                vec![
                    Value::Int64(1),
                    Value::Float64(f64::NAN),
                    Value::Bool(false),
                    Value::String("bad".to_owned()),
                ],
                Error::NonFiniteFloat {
                    table: "events".to_owned(),
                    column: "score".to_owned(),
                },
            ),
            (
                vec![
                    Value::Int64(1),
                    Value::Float64(f64::INFINITY),
                    Value::Bool(false),
                    Value::String("bad".to_owned()),
                ],
                Error::NonFiniteFloat {
                    table: "events".to_owned(),
                    column: "score".to_owned(),
                },
            ),
            (
                vec![
                    Value::Int64(1),
                    Value::Float64(f64::NEG_INFINITY),
                    Value::Bool(false),
                    Value::String("bad".to_owned()),
                ],
                Error::NonFiniteFloat {
                    table: "events".to_owned(),
                    column: "score".to_owned(),
                },
            ),
        ];

        for (row, expected_error) in invalid_rows {
            assert_eq!(table.insert_row(row), Err(expected_error));
            assert_eq!(table, original);
            assert!(table.columns().iter().all(Column::is_empty));
        }
    }
}
