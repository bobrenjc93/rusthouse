use std::collections::HashSet;

use crate::{DataType, Error, Result, Value};

/// Configurable resource limits for one in-memory [`Table`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageLimits {
    /// Maximum number of physical columns.
    pub max_columns: usize,
    /// Maximum number of stored rows.
    pub max_rows: usize,
    /// Maximum UTF-8 byte length of one string value.
    pub max_string_bytes: usize,
    /// Maximum aggregate bytes in stored scalar values.
    ///
    /// This accounts for scalar payloads, not `Vec` capacities or allocator
    /// metadata.
    pub max_total_value_bytes: usize,
}

impl StorageLimits {
    /// Bounded defaults used by [`Table::new`].
    pub const DEFAULT: Self = Self {
        max_columns: 100_000,
        max_rows: 10_000_000,
        max_string_bytes: 1024 * 1024,
        max_total_value_bytes: 1024 * 1024 * 1024,
    };

    /// Creates explicit column, row, string, and aggregate value-byte limits.
    #[must_use]
    pub const fn new(
        max_columns: usize,
        max_rows: usize,
        max_string_bytes: usize,
        max_total_value_bytes: usize,
    ) -> Self {
        Self {
            max_columns,
            max_rows,
            max_string_bytes,
            max_total_value_bytes,
        }
    }
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

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
    value_bytes: usize,
    limits: StorageLimits,
}

impl Table {
    /// Creates an empty table with a validated schema.
    ///
    /// Returns [`Error::EmptySchema`] for a schema without fields and
    /// [`Error::DuplicateColumn`] when names repeat, ignoring ASCII case.
    /// [`StorageLimits::DEFAULT`] bounds the table's columns and values.
    pub fn new(name: impl Into<String>, schema: Vec<ColumnDef>) -> Result<Self> {
        Self::new_with_limits(name, schema, StorageLimits::default())
    }

    /// Creates an empty table with a validated schema and explicit limits.
    pub fn new_with_limits(
        name: impl Into<String>,
        schema: Vec<ColumnDef>,
        limits: StorageLimits,
    ) -> Result<Self> {
        if schema.is_empty() {
            return Err(Error::EmptySchema);
        }
        if schema.len() > limits.max_columns {
            return Err(Error::ColumnLimitExceeded {
                limit: limits.max_columns,
                actual: schema.len(),
            });
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
            value_bytes: 0,
            limits,
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

    /// Returns the configured resource limits.
    #[must_use]
    pub const fn limits(&self) -> StorageLimits {
        self.limits
    }

    /// Returns the aggregate bytes in stored scalar values.
    ///
    /// This excludes vector capacities and allocator metadata.
    #[must_use]
    pub const fn value_bytes(&self) -> usize {
        self.value_bytes
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

    /// Validates that a row could be inserted without changing the table.
    ///
    /// This checks its shape, value types, and the table's row and value-byte
    /// limits as though it were the next inserted row.
    pub fn validate_row(&self, row: &[Value]) -> Result<()> {
        let row_bytes = self.validate_row_values(row)?;
        self.ensure_insert_capacity(1, row_bytes)
    }

    fn validate_row_values(&self, row: &[Value]) -> Result<usize> {
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
            if let Value::String(string) = value {
                if string.len() > self.limits.max_string_bytes {
                    return Err(Error::StringValueLimitExceeded {
                        table: self.name.clone(),
                        column: field.name.clone(),
                        limit: self.limits.max_string_bytes,
                        actual: string.len(),
                    });
                }
            }
        }

        Ok(row.iter().fold(0usize, |total, value| {
            total.saturating_add(value.storage_bytes())
        }))
    }

    fn ensure_insert_capacity(&self, rows: usize, value_bytes: usize) -> Result<()> {
        let row_count = self.row_count.saturating_add(rows);
        if row_count > self.limits.max_rows {
            return Err(Error::RowLimitExceeded {
                table: self.name.clone(),
                limit: self.limits.max_rows,
                actual: row_count,
            });
        }

        let total_value_bytes = self.value_bytes.saturating_add(value_bytes);
        if total_value_bytes > self.limits.max_total_value_bytes {
            return Err(Error::ValueStorageLimitExceeded {
                table: self.name.clone(),
                limit: self.limits.max_total_value_bytes,
                actual: total_value_bytes,
            });
        }

        Ok(())
    }

    /// Validates and appends one complete row to the table.
    ///
    /// No column is changed when validation returns an error.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        let row_bytes = self.validate_row_values(&row)?;
        self.ensure_insert_capacity(1, row_bytes)?;
        self.append_row(row);
        self.value_bytes += row_bytes;
        Ok(())
    }

    /// Validates and appends a batch of complete rows to the table.
    ///
    /// All rows are validated before any value is appended. [`Error::BatchRow`]
    /// reports the zero-based index of a row with invalid values. Batch-wide
    /// row or aggregate value-byte limit errors are returned directly. Every
    /// failure leaves the table unchanged.
    pub fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        let mut batch_value_bytes = 0usize;
        for (row_index, row) in rows.iter().enumerate() {
            let row_bytes = self
                .validate_row_values(row)
                .map_err(|source| Error::BatchRow {
                    row_index,
                    source: Box::new(source),
                })?;
            batch_value_bytes = batch_value_bytes.saturating_add(row_bytes);
        }
        self.ensure_insert_capacity(rows.len(), batch_value_bytes)?;

        for row in rows {
            self.append_row(row);
        }
        self.value_bytes += batch_value_bytes;
        Ok(())
    }

    fn append_row(&mut self, row: Vec<Value>) {
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
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

    #[test]
    fn inserts_empty_and_valid_batches() {
        let mut table = four_type_table();

        table.insert_rows(vec![]).expect("empty batch");
        assert!(table.is_empty());
        assert!(table.columns().iter().all(Column::is_empty));

        table
            .insert_rows(vec![
                vec![
                    Value::Int64(1),
                    Value::Float64(1.5),
                    Value::Bool(true),
                    Value::String("first".to_owned()),
                ],
                vec![
                    Value::Int64(2),
                    Value::Float64(2.5),
                    Value::Bool(false),
                    Value::String("second".to_owned()),
                ],
                vec![
                    Value::Int64(3),
                    Value::Float64(3.5),
                    Value::Bool(true),
                    Value::String("third".to_owned()),
                ],
            ])
            .expect("valid batch");

        assert_eq!(table.row_count(), 3);
        assert!(matches!(&table.columns()[0], Column::Int64(v) if v == &[1, 2, 3]));
        assert!(matches!(&table.columns()[1], Column::Float64(v) if v == &[1.5, 2.5, 3.5]));
        assert!(matches!(&table.columns()[2], Column::Bool(v) if v == &[true, false, true]));
        assert!(
            matches!(&table.columns()[3], Column::String(v) if v == &["first", "second", "third"])
        );
    }

    #[test]
    fn rejected_batches_report_the_row_and_leave_every_column_unchanged() {
        let valid_row = || {
            vec![
                Value::Int64(10),
                Value::Float64(10.5),
                Value::Bool(true),
                Value::String("valid".to_owned()),
            ]
        };
        let invalid_values = [
            Value::String("not an integer".to_owned()),
            Value::Bool(false),
            Value::Int64(0),
            Value::Float64(0.0),
        ];
        let expected_types = [
            DataType::Int64,
            DataType::Float64,
            DataType::Bool,
            DataType::String,
        ];
        let actual_types = [
            DataType::String,
            DataType::Bool,
            DataType::Int64,
            DataType::Float64,
        ];
        let column_names = ["id", "score", "enabled", "label"];

        for row_index in [0, 1, 2] {
            for column_index in 0..4 {
                let mut table = four_type_table();
                table.insert_row(valid_row()).expect("seed row");
                let original = table.clone();
                let mut rows = vec![valid_row(), valid_row(), valid_row()];
                rows[row_index][column_index] = invalid_values[column_index].clone();

                assert_eq!(
                    table.insert_rows(rows),
                    Err(Error::BatchRow {
                        row_index,
                        source: Box::new(Error::TypeMismatch {
                            table: "events".to_owned(),
                            column: column_names[column_index].to_owned(),
                            expected: expected_types[column_index],
                            actual: actual_types[column_index],
                        }),
                    })
                );
                assert_eq!(table.row_count(), original.row_count());
                assert_eq!(table.columns(), original.columns());
                assert_eq!(table, original);
            }
        }
    }
}
