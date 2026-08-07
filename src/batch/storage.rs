use std::collections::{HashMap, HashSet};

use crate::batch::error::{Error, Result};
use crate::batch::value::{DataType, Value, ValueRef};

/// Default maximum number of rows retained by one typed batch table.
pub const DEFAULT_MAX_ROWS_PER_TABLE: usize = 1_000_000;

/// A named, typed field in a table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
}

pub(crate) fn is_sql_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

pub(crate) fn is_sql_identifier_continue(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn is_valid_sql_identifier(identifier: &str) -> bool {
    let mut characters = identifier.chars();
    characters.next().is_some_and(is_sql_identifier_start)
        && characters.all(is_sql_identifier_continue)
}

fn validate_sql_identifier(identifier: &str, context: &str) -> Result<()> {
    if is_valid_sql_identifier(identifier) {
        Ok(())
    } else {
        Err(Error::InvalidIdentifier {
            identifier: identifier.to_owned(),
            context: context.to_owned(),
        })
    }
}

pub(crate) fn validate_table_name(name: &str) -> Result<()> {
    validate_sql_identifier(name, "table name")
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

    fn clear(&mut self) {
        match self {
            Self::Int64(values) => values.clear(),
            Self::Float64(values) => values.clear(),
            Self::Bool(values) => values.clear(),
            Self::String(values) => values.clear(),
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
    row_cap: usize,
}

impl Table {
    /// Creates an empty table with the finite default row cap.
    pub fn new(name: String, schema: Vec<ColumnDef>) -> Result<Self> {
        Self::with_row_cap(name, schema, DEFAULT_MAX_ROWS_PER_TABLE)
    }

    /// Creates an empty table with an explicit maximum retained row count.
    pub fn with_row_cap(name: String, schema: Vec<ColumnDef>, row_cap: usize) -> Result<Self> {
        validate_table_name(&name)?;
        if schema.is_empty() {
            return Err(Error::InvalidQuery(
                "a table must contain at least one column".to_owned(),
            ));
        }
        let mut column_names = HashSet::with_capacity(schema.len());
        for field in &schema {
            validate_sql_identifier(&field.name, "column name")?;
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
            row_cap,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Changes only the display name after the catalog has preflighted a rename.
    pub(crate) fn set_name(&mut self, name: String) {
        self.name = name;
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

    /// Returns the maximum number of rows this table can retain.
    #[must_use]
    pub fn row_cap(&self) -> usize {
        self.row_cap
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

    /// Resolves a complete explicit INSERT column list and returns rows in
    /// physical schema order. Positional rows are returned unchanged.
    ///
    /// Explicit names resolve case-insensitively. Every schema column must be
    /// named exactly once; unknown, duplicate, and omitted names are rejected
    /// before any row is changed.
    pub(crate) fn prepare_insert_rows(
        &self,
        insert_columns: Option<&[String]>,
        mut rows: Vec<Vec<Value>>,
    ) -> Result<Vec<Vec<Value>>> {
        let Some(insert_columns) = insert_columns else {
            return Ok(rows);
        };

        let schema_indexes_by_name = self
            .schema
            .iter()
            .enumerate()
            .map(|(index, field)| (field.name.to_ascii_lowercase(), index))
            .collect::<HashMap<_, _>>();
        let mut schema_indexes = Vec::with_capacity(insert_columns.len());
        let mut seen = vec![false; self.schema.len()];
        for name in insert_columns {
            let Some(&schema_index) = schema_indexes_by_name.get(&name.to_ascii_lowercase()) else {
                return Err(Error::ColumnNotFound {
                    table: self.name.clone(),
                    column: name.clone(),
                });
            };
            if std::mem::replace(&mut seen[schema_index], true) {
                return Err(Error::DuplicateColumn(name.clone()));
            }
            schema_indexes.push(schema_index);
        }
        if let Some((index, _)) = seen.iter().enumerate().find(|(_, present)| !**present) {
            return Err(Error::MissingInsertColumn {
                table: self.name.clone(),
                column: self.schema[index].name.clone(),
            });
        }

        for row in &rows {
            if row.len() != self.schema.len() {
                return Err(Error::RowLength {
                    table: self.name.clone(),
                    expected: self.schema.len(),
                    actual: row.len(),
                });
            }
        }

        if schema_indexes.iter().copied().eq(0..self.schema.len()) {
            return Ok(rows);
        }
        for row in &mut rows {
            let source = std::mem::take(row);
            let mut reordered = Vec::with_capacity(source.len());
            reordered.resize_with(source.len(), || None);
            for (value, schema_index) in source.into_iter().zip(schema_indexes.iter().copied()) {
                reordered[schema_index] = Some(value);
            }
            *row = reordered
                .into_iter()
                .map(|value| value.expect("complete INSERT columns cover every schema position"))
                .collect();
        }
        Ok(rows)
    }

    /// Changes only a column's display name after validating the complete rename.
    ///
    /// Source and collision checks are case-insensitive. Renaming a column to
    /// another spelling of its own name is allowed, while invalid, reserved,
    /// and already-used destinations leave the schema unchanged.
    pub fn rename_column(&mut self, source: &str, destination: String) -> Result<()> {
        let source_index = self.column_index(source)?;
        validate_sql_identifier(&destination, "column name")?;
        if is_reserved_column_name(&destination) {
            return Err(Error::ReservedIdentifier {
                identifier: destination,
                context: "column name".to_owned(),
            });
        }
        if self.schema.iter().enumerate().any(|(index, field)| {
            index != source_index && field.name.eq_ignore_ascii_case(&destination)
        }) {
            return Err(Error::DuplicateColumn(destination));
        }

        self.schema[source_index].name = destination;
        Ok(())
    }

    /// Appends one schema field and an aligned physical column of defaults.
    ///
    /// Name validation and case-insensitive collision detection complete
    /// before either the schema or physical columns are changed. Existing
    /// rows receive ClickHouse-style non-null defaults for the new type.
    pub fn add_column(&mut self, field: ColumnDef) -> Result<()> {
        validate_sql_identifier(&field.name, "column name")?;
        if is_reserved_column_name(&field.name) {
            return Err(Error::ReservedIdentifier {
                identifier: field.name,
                context: "column name".to_owned(),
            });
        }
        if self
            .schema
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&field.name))
        {
            return Err(Error::DuplicateColumn(field.name));
        }

        let column = match field.data_type {
            DataType::Int64 => Column::Int64(vec![0; self.row_count]),
            DataType::Float64 => Column::Float64(vec![0.0; self.row_count]),
            DataType::Bool => Column::Bool(vec![false; self.row_count]),
            DataType::String => Column::String(vec![String::new(); self.row_count]),
        };

        debug_assert_eq!(self.schema.len(), self.columns.len());
        self.schema.push(field);
        self.columns.push(column);
        Ok(())
    }

    /// Removes one schema field and its aligned physical column.
    ///
    /// Resolution is case-insensitive. The column lookup and the invariant
    /// that a table retains at least one column are checked before either
    /// vector is changed.
    pub fn drop_column(&mut self, column: &str) -> Result<()> {
        let column_index = self.column_index(column)?;
        if self.schema.len() == 1 {
            return Err(Error::InvalidQuery(format!(
                "cannot drop the only column from table '{}'",
                self.name
            )));
        }

        debug_assert_eq!(self.schema.len(), self.columns.len());
        self.schema.remove(column_index);
        self.columns.remove(column_index);
        Ok(())
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
            if matches!(value, Value::Null(_)) {
                return Err(Error::TypeMismatch {
                    context: format!("column '{}.{}'", self.name, field.name),
                    expected: field.data_type.to_string(),
                    actual: "NULL".to_owned(),
                });
            }
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

    /// Validates the row cap and complete row before appending to any column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.validate_row_capacity(1)?;
        self.validate_row(&row)?;
        self.append_validated_row(row);
        Ok(())
    }

    /// Atomically validates and appends a complete batch of rows.
    pub fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        self.validate_row_capacity(rows.len())?;
        for row in &rows {
            self.validate_row(row)?;
        }
        self.append_validated_rows(rows);
        Ok(())
    }

    pub(crate) fn validate_row_capacity(&self, incoming_rows: usize) -> Result<()> {
        if incoming_rows > self.row_cap.saturating_sub(self.row_count) {
            return Err(Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: self.row_count.saturating_add(incoming_rows),
                max: self.row_cap,
            });
        }
        Ok(())
    }

    pub(crate) fn append_validated_rows(&mut self, rows: Vec<Vec<Value>>) {
        for row in rows {
            self.append_validated_row(row);
        }
    }

    fn append_validated_row(&mut self, row: Vec<Value>) {
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
    }

    /// Removes every row while retaining the table name, schema, and physical columns.
    pub fn truncate(&mut self) -> usize {
        let removed_rows = self.row_count;
        for column in &mut self.columns {
            column.clear();
        }
        self.row_count = 0;
        removed_rows
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
    fn rejected_row_batch_does_not_mutate_at_the_row_cap() {
        let mut table = Table::with_row_cap(
            "events".to_owned(),
            vec![ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            }],
            2,
        )
        .expect("valid schema");
        table
            .insert_row(vec![Value::Int64(1)])
            .expect("first row fits");

        assert_eq!(
            table.insert_rows(vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]),
            Err(Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 3,
                max: 2,
            })
        );
        assert_eq!(table.row_count(), 1);
        assert!(matches!(&table.columns()[0], Column::Int64(v) if v == &[1]));
    }
}
