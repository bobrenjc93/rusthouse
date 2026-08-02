use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::error::{Error, Result};
use crate::sql::{ColumnDefinition, CreateTable};

pub(crate) const MAX_COLUMNS: usize = 1_024;
pub(crate) const MAX_ROWS: usize = 1_000_000;
const MAX_TABLE_CELLS: usize = 10_000_000;
const MAX_TABLE_BYTES: usize = 128 * 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 256 * 1024 * 1024;
const MAX_INSERT_STAGING_BYTES: usize = 128 * 1024 * 1024;

/// A physical column type supported by RustHouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        })
    }
}

/// A scalar SQL value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Value {
    pub(crate) fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Null => None,
            Self::Int64(_) => Some(DataType::Int64),
            Self::Float64(_) => Some(DataType::Float64),
            Self::Bool(_) => Some(DataType::Bool),
            Self::String(_) => Some(DataType::String),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Int64(value) => write!(f, "{value}"),
            Self::Float64(value) => write!(f, "{value}"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::String(value) => f.write_str(value),
        }
    }
}

/// The name and type constraints of one table column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSchema {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum ColumnData {
    Int64 {
        values: Vec<i64>,
        nulls: Vec<bool>,
    },
    Float64 {
        values: Vec<f64>,
        nulls: Vec<bool>,
    },
    Bool {
        values: Vec<bool>,
        nulls: Vec<bool>,
    },
    String {
        values: Vec<String>,
        nulls: Vec<bool>,
    },
}

impl ColumnData {
    fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64 {
                values: Vec::new(),
                nulls: Vec::new(),
            },
            DataType::Float64 => Self::Float64 {
                values: Vec::new(),
                nulls: Vec::new(),
            },
            DataType::Bool => Self::Bool {
                values: Vec::new(),
                nulls: Vec::new(),
            },
            DataType::String => Self::String {
                values: Vec::new(),
                nulls: Vec::new(),
            },
        }
    }

    pub(crate) fn value(&self, row: usize) -> Value {
        match self {
            Self::Int64 { values, nulls } => {
                if nulls[row] {
                    Value::Null
                } else {
                    Value::Int64(values[row])
                }
            }
            Self::Float64 { values, nulls } => {
                if nulls[row] {
                    Value::Null
                } else {
                    Value::Float64(values[row])
                }
            }
            Self::Bool { values, nulls } => {
                if nulls[row] {
                    Value::Null
                } else {
                    Value::Bool(values[row])
                }
            }
            Self::String { values, nulls } => {
                if nulls[row] {
                    Value::Null
                } else {
                    Value::String(values[row].clone())
                }
            }
        }
    }

    fn value_size(&self, row: usize) -> usize {
        let payload = match self {
            Self::String { values, nulls } if !nulls[row] => values[row].len(),
            _ => 0,
        };
        std::mem::size_of::<Value>() + payload
    }

    fn append(&mut self, value: Value) {
        let is_null = matches!(value, Value::Null);
        match (self, value) {
            (Self::Int64 { values, nulls }, Value::Int64(value)) => {
                values.push(value);
                nulls.push(false);
            }
            (Self::Int64 { values, nulls }, Value::Null) => {
                values.push(0);
                nulls.push(true);
            }
            (Self::Float64 { values, nulls }, Value::Float64(value)) => {
                values.push(value);
                nulls.push(false);
            }
            (Self::Float64 { values, nulls }, Value::Null) => {
                values.push(0.0);
                nulls.push(true);
            }
            (Self::Bool { values, nulls }, Value::Bool(value)) => {
                values.push(value);
                nulls.push(false);
            }
            (Self::Bool { values, nulls }, Value::Null) => {
                values.push(false);
                nulls.push(true);
            }
            (Self::String { values, nulls }, Value::String(value)) => {
                values.push(value);
                nulls.push(false);
            }
            (Self::String { values, nulls }, Value::Null) => {
                values.push(String::new());
                nulls.push(true);
            }
            _ => unreachable!("values are coerced before append (null={is_null})"),
        }
    }

    fn try_reserve(&mut self, additional: usize) -> Result<()> {
        let reserve = |error: std::collections::TryReserveError| {
            Error::new(format!("unable to reserve column storage: {error}"))
        };
        match self {
            Self::Int64 { values, nulls } => {
                values.try_reserve(additional).map_err(reserve)?;
                nulls.try_reserve(additional).map_err(reserve)
            }
            Self::Float64 { values, nulls } => {
                values.try_reserve(additional).map_err(reserve)?;
                nulls.try_reserve(additional).map_err(reserve)
            }
            Self::Bool { values, nulls } => {
                values.try_reserve(additional).map_err(reserve)?;
                nulls.try_reserve(additional).map_err(reserve)
            }
            Self::String { values, nulls } => {
                values.try_reserve(additional).map_err(reserve)?;
                nulls.try_reserve(additional).map_err(reserve)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Table {
    pub schema: Vec<ColumnSchema>,
    pub columns: Vec<ColumnData>,
    pub row_count: usize,
    storage_bytes: usize,
    column_indexes: HashMap<String, usize>,
}

impl Table {
    pub(crate) fn column_index(&self, name: &str) -> Option<usize> {
        self.column_indexes.get(&canonical(name)).copied()
    }

    pub(crate) fn value(&self, column: usize, row: usize) -> Value {
        self.columns[column].value(row)
    }

    pub(crate) fn value_size(&self, column: usize, row: usize) -> usize {
        self.columns[column].value_size(row)
    }

    fn insert(
        &mut self,
        column_names: Option<&[String]>,
        rows: Vec<Vec<Value>>,
        catalog_available_bytes: usize,
        catalog_limit: usize,
    ) -> Result<(usize, usize)> {
        if rows.is_empty() {
            return Err(Error::new("INSERT must contain at least one row"));
        }
        let new_count = self
            .row_count
            .checked_add(rows.len())
            .ok_or_else(|| Error::new("table row count overflow"))?;
        if new_count > MAX_ROWS {
            return Err(Error::new(format!(
                "table row limit exceeded (maximum {MAX_ROWS})"
            )));
        }

        let targets = self.insert_targets(column_names)?;
        let table_cells = new_count
            .checked_mul(self.schema.len())
            .ok_or_else(|| Error::new("table cell count overflow"))?;
        if table_cells > MAX_TABLE_CELLS {
            return Err(Error::new(format!(
                "table cell limit exceeded (maximum {MAX_TABLE_CELLS})"
            )));
        }

        let mut target_positions = vec![None; self.schema.len()];
        for (position, &target) in targets.iter().enumerate() {
            target_positions[target] = Some(position);
        }
        for (index, schema) in self.schema.iter().enumerate() {
            if target_positions[index].is_none() && !schema.nullable {
                return Err(Error::new(format!(
                    "INSERT omits non-nullable column '{}'",
                    schema.name
                )));
            }
        }

        let row_storage_bytes = self.schema.iter().try_fold(0usize, |total, schema| {
            total
                .checked_add(base_storage_bytes(schema.data_type))
                .ok_or_else(|| Error::new("table storage size overflow"))
        })?;
        let mut insert_bytes = row_storage_bytes
            .checked_mul(rows.len())
            .ok_or_else(|| Error::new("table storage size overflow"))?;
        check_table_bytes(self.storage_bytes, insert_bytes)?;
        check_catalog_bytes(insert_bytes, catalog_available_bytes, catalog_limit)?;

        let staged_cells = rows
            .len()
            .checked_mul(targets.len())
            .ok_or_else(|| Error::new("INSERT staging size overflow"))?;
        let staging_bytes = staged_cells
            .checked_mul(std::mem::size_of::<Value>())
            .ok_or_else(|| Error::new("INSERT staging size overflow"))?;
        if staging_bytes > MAX_INSERT_STAGING_BYTES {
            return Err(Error::new(format!(
                "INSERT staging limit exceeded (maximum {MAX_INSERT_STAGING_BYTES} bytes)"
            )));
        }

        let mut staged = targets
            .iter()
            .map(|_| {
                let mut column = Vec::new();
                column.try_reserve(rows.len()).map_err(|error| {
                    Error::new(format!("unable to reserve INSERT staging: {error}"))
                })?;
                Ok(column)
            })
            .collect::<Result<Vec<Vec<Value>>>>()?;
        for (row_index, row) in rows.into_iter().enumerate() {
            if row.len() != targets.len() {
                return Err(Error::new(format!(
                    "INSERT row {} has {} values but {} were expected",
                    row_index + 1,
                    row.len(),
                    targets.len()
                )));
            }
            for (input_position, (value, &target)) in row.into_iter().zip(&targets).enumerate() {
                let value = coerce(
                    value,
                    &self.schema[target],
                    row_index + 1,
                    input_position + 1,
                )?;
                if let Value::String(value) = &value {
                    insert_bytes = insert_bytes
                        .checked_add(value.len())
                        .ok_or_else(|| Error::new("table storage size overflow"))?;
                }
                staged[input_position].push(value);
            }
        }
        check_table_bytes(self.storage_bytes, insert_bytes)?;
        check_catalog_bytes(insert_bytes, catalog_available_bytes, catalog_limit)?;

        // Reservations may change capacities, but table values remain untouched on failure.
        let inserted = staged.first().map_or(0, Vec::len);
        for column in &mut self.columns {
            column.try_reserve(inserted)?;
        }
        // All fallible work is complete; mutations below commit the full statement.
        for (column_index, column) in self.columns.iter_mut().enumerate() {
            if let Some(position) = target_positions[column_index] {
                for value in std::mem::take(&mut staged[position]) {
                    column.append(value);
                }
            } else {
                for _ in 0..inserted {
                    column.append(Value::Null);
                }
            }
        }
        self.row_count = new_count;
        self.storage_bytes += insert_bytes;
        Ok((inserted, insert_bytes))
    }

    fn insert_targets(&self, column_names: Option<&[String]>) -> Result<Vec<usize>> {
        let Some(names) = column_names else {
            return Ok((0..self.schema.len()).collect());
        };
        if names.is_empty() {
            return Err(Error::new("INSERT column list cannot be empty"));
        }
        let mut seen = HashSet::new();
        names
            .iter()
            .map(|name| {
                let key = canonical(name);
                if !seen.insert(key) {
                    return Err(Error::new(format!(
                        "column '{name}' appears more than once in INSERT"
                    )));
                }
                self.column_index(name)
                    .ok_or_else(|| Error::new(format!("unknown column '{name}'")))
            })
            .collect()
    }
}

fn base_storage_bytes(data_type: DataType) -> usize {
    let value = match data_type {
        DataType::Int64 => std::mem::size_of::<i64>(),
        DataType::Float64 => std::mem::size_of::<f64>(),
        DataType::Bool => std::mem::size_of::<bool>(),
        DataType::String => std::mem::size_of::<String>(),
    };
    value + std::mem::size_of::<bool>()
}

fn check_table_bytes(existing: usize, additional: usize) -> Result<()> {
    let total = existing
        .checked_add(additional)
        .ok_or_else(|| Error::new("table storage size overflow"))?;
    if total > MAX_TABLE_BYTES {
        Err(Error::new(format!(
            "table storage byte limit exceeded (maximum {MAX_TABLE_BYTES} bytes)"
        )))
    } else {
        Ok(())
    }
}

fn check_catalog_bytes(additional: usize, available: usize, limit: usize) -> Result<()> {
    if additional > available {
        Err(Error::new(format!(
            "catalog storage byte limit exceeded (maximum {limit} bytes)"
        )))
    } else {
        Ok(())
    }
}

fn coerce(value: Value, schema: &ColumnSchema, row: usize, position: usize) -> Result<Value> {
    if value == Value::Null {
        return if schema.nullable {
            Ok(value)
        } else {
            Err(Error::new(format!(
                "INSERT row {row}, value {position}: column '{}' is not nullable",
                schema.name
            )))
        };
    }
    let actual = value.data_type().expect("non-null value has a type");
    match (schema.data_type, value) {
        (DataType::Int64, value @ Value::Int64(_))
        | (DataType::Float64, value @ Value::Float64(_))
        | (DataType::Bool, value @ Value::Bool(_))
        | (DataType::String, value @ Value::String(_)) => Ok(value),
        (DataType::Float64, Value::Int64(value)) => Ok(Value::Float64(value as f64)),
        _ => Err(Error::new(format!(
            "INSERT row {row}, value {position}: column '{}' expects {} but got {actual}",
            schema.name, schema.data_type
        ))),
    }
}

pub(crate) struct Catalog {
    tables: HashMap<String, Table>,
    storage_bytes: usize,
    storage_limit: usize,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            tables: HashMap::new(),
            storage_bytes: 0,
            storage_limit: MAX_CATALOG_BYTES,
        }
    }
}

impl Catalog {
    pub(crate) fn create(&mut self, statement: CreateTable) -> Result<()> {
        let table_key = canonical(&statement.name);
        if self.tables.contains_key(&table_key) {
            return if statement.if_not_exists {
                Ok(())
            } else {
                Err(Error::new(format!(
                    "table '{}' already exists",
                    statement.name
                )))
            };
        }
        validate_columns(&statement.columns)?;
        let schema: Vec<_> = statement
            .columns
            .into_iter()
            .map(|column| ColumnSchema {
                name: column.name,
                data_type: column.data_type,
                nullable: column.nullable,
            })
            .collect();
        let column_indexes = schema
            .iter()
            .enumerate()
            .map(|(index, column)| (canonical(&column.name), index))
            .collect();
        let columns = schema
            .iter()
            .map(|column| ColumnData::new(column.data_type))
            .collect();
        self.tables.insert(
            table_key,
            Table {
                schema,
                columns,
                row_count: 0,
                storage_bytes: 0,
                column_indexes,
            },
        );
        Ok(())
    }

    pub(crate) fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(&canonical(name))
            .ok_or_else(|| Error::new(format!("unknown table '{name}'")))
    }

    pub(crate) fn insert(
        &mut self,
        table: &str,
        columns: Option<&[String]>,
        rows: Vec<Vec<Value>>,
    ) -> Result<usize> {
        let available = self.storage_limit.saturating_sub(self.storage_bytes);
        let (inserted, inserted_bytes) = self
            .tables
            .get_mut(&canonical(table))
            .ok_or_else(|| Error::new(format!("unknown table '{table}'")))?
            .insert(columns, rows, available, self.storage_limit)?;
        self.storage_bytes += inserted_bytes;
        Ok(inserted)
    }

    #[cfg(test)]
    fn with_storage_limit(storage_limit: usize) -> Self {
        Self {
            tables: HashMap::new(),
            storage_bytes: 0,
            storage_limit,
        }
    }
}

fn validate_columns(columns: &[ColumnDefinition]) -> Result<()> {
    if columns.is_empty() {
        return Err(Error::new("a table must have at least one column"));
    }
    if columns.len() > MAX_COLUMNS {
        return Err(Error::new(format!(
            "column limit exceeded (maximum {MAX_COLUMNS})"
        )));
    }
    let mut names = HashSet::new();
    for column in columns {
        if !names.insert(canonical(&column.name)) {
            return Err(Error::new(format!(
                "duplicate column name '{}'",
                column.name
            )));
        }
    }
    Ok(())
}

pub(crate) fn canonical(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog
            .create(CreateTable {
                name: "t".to_owned(),
                if_not_exists: false,
                columns: vec![
                    ColumnDefinition {
                        name: "id".to_owned(),
                        data_type: DataType::Int64,
                        nullable: false,
                    },
                    ColumnDefinition {
                        name: "score".to_owned(),
                        data_type: DataType::Float64,
                        nullable: true,
                    },
                ],
            })
            .unwrap();
        catalog
    }

    #[test]
    fn multi_row_insert_is_atomic() {
        let mut catalog = catalog();
        let error = catalog
            .insert(
                "t",
                None,
                vec![
                    vec![Value::Int64(1), Value::Float64(1.5)],
                    vec![Value::String("bad".to_owned()), Value::Null],
                ],
            )
            .unwrap_err();
        assert!(error.message().contains("expects Int64"));
        assert_eq!(catalog.table("t").unwrap().row_count, 0);
        assert!(catalog.table("t").unwrap().columns.iter().all(|column| {
            match column {
                ColumnData::Int64 { values, .. } => values.is_empty(),
                ColumnData::Float64 { values, .. } => values.is_empty(),
                _ => false,
            }
        }));
    }

    #[test]
    fn omitted_nullable_columns_are_null() {
        let mut catalog = catalog();
        catalog
            .insert("t", Some(&["id".to_owned()]), vec![vec![Value::Int64(1)]])
            .unwrap();
        assert_eq!(catalog.table("t").unwrap().value(1, 0), Value::Null);
    }

    #[test]
    fn compact_inserts_obey_table_cell_and_byte_limits() {
        fn wide_catalog(name: &str, columns: usize, data_type: DataType) -> Catalog {
            let mut catalog = Catalog::default();
            catalog
                .create(CreateTable {
                    name: name.to_owned(),
                    if_not_exists: false,
                    columns: (0..columns)
                        .map(|index| ColumnDefinition {
                            name: format!("c{index}"),
                            data_type,
                            nullable: true,
                        })
                        .collect(),
                })
                .unwrap();
            catalog
        }

        let mut cells = wide_catalog("cells", MAX_COLUMNS, DataType::Bool);
        let cell_rows = MAX_TABLE_CELLS / MAX_COLUMNS + 1;
        let error = cells
            .insert(
                "cells",
                Some(&["c0".to_owned()]),
                vec![vec![Value::Bool(true)]; cell_rows],
            )
            .unwrap_err();
        assert!(error.message().contains("table cell limit"));
        assert_eq!(cells.table("cells").unwrap().row_count, 0);

        let string_columns = 512;
        let bytes_per_row = string_columns * base_storage_bytes(DataType::String);
        let byte_rows = MAX_TABLE_BYTES / bytes_per_row + 1;
        let mut bytes = wide_catalog("bytes", string_columns, DataType::String);
        let error = bytes
            .insert(
                "bytes",
                Some(&["c0".to_owned()]),
                vec![vec![Value::String(String::new())]; byte_rows],
            )
            .unwrap_err();
        assert!(error.message().contains("storage byte limit"));
        assert_eq!(bytes.table("bytes").unwrap().row_count, 0);
    }

    #[test]
    fn catalog_storage_limit_is_cumulative_across_tables() {
        let bytes_per_value = base_storage_bytes(DataType::String);
        let mut catalog = Catalog::with_storage_limit(bytes_per_value * 3);
        for name in ["first", "second"] {
            catalog
                .create(CreateTable {
                    name: name.to_owned(),
                    if_not_exists: false,
                    columns: vec![ColumnDefinition {
                        name: "value".to_owned(),
                        data_type: DataType::String,
                        nullable: true,
                    }],
                })
                .unwrap();
        }
        catalog
            .insert("first", None, vec![vec![Value::Null], vec![Value::Null]])
            .unwrap();
        let error = catalog
            .insert("second", None, vec![vec![Value::Null], vec![Value::Null]])
            .unwrap_err();
        assert!(error.message().contains("catalog storage byte limit"));
        assert_eq!(catalog.table("first").unwrap().row_count, 2);
        assert_eq!(catalog.table("second").unwrap().row_count, 0);
        assert_eq!(catalog.storage_bytes, bytes_per_value * 2);
    }
}
