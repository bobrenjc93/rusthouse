use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::error::{Error, Result};
use crate::sql::{ColumnDefinition, CreateTable};

pub(crate) const MAX_COLUMNS: usize = 1_024;
pub(crate) const MAX_ROWS: usize = 1_000_000;

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
}

#[derive(Clone, Debug)]
pub(crate) struct Table {
    pub schema: Vec<ColumnSchema>,
    pub columns: Vec<ColumnData>,
    pub row_count: usize,
    column_indexes: HashMap<String, usize>,
}

impl Table {
    pub(crate) fn column_index(&self, name: &str) -> Option<usize> {
        self.column_indexes.get(&canonical(name)).copied()
    }

    pub(crate) fn value(&self, column: usize, row: usize) -> Value {
        self.columns[column].value(row)
    }

    fn insert(&mut self, column_names: Option<&[String]>, rows: Vec<Vec<Value>>) -> Result<usize> {
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
        let mut staged = Vec::with_capacity(rows.len());
        for (row_index, row) in rows.into_iter().enumerate() {
            if row.len() != targets.len() {
                return Err(Error::new(format!(
                    "INSERT row {} has {} values but {} were expected",
                    row_index + 1,
                    row.len(),
                    targets.len()
                )));
            }
            let mut complete = vec![Value::Null; self.schema.len()];
            for ((value, &target), input_position) in row.into_iter().zip(&targets).zip(0usize..) {
                complete[target] = coerce(
                    value,
                    &self.schema[target],
                    row_index + 1,
                    input_position + 1,
                )?;
            }
            for (column_index, schema) in self.schema.iter().enumerate() {
                if complete[column_index] == Value::Null && !schema.nullable {
                    return Err(Error::new(format!(
                        "INSERT row {} omits non-nullable column '{}'",
                        row_index + 1,
                        schema.name
                    )));
                }
            }
            staged.push(complete);
        }

        // All fallible work is complete; mutations below commit the full statement.
        let inserted = staged.len();
        for row in staged {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.append(value);
            }
        }
        self.row_count = new_count;
        Ok(inserted)
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

#[derive(Default)]
pub(crate) struct Catalog {
    tables: HashMap<String, Table>,
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
        self.tables
            .get_mut(&canonical(table))
            .ok_or_else(|| Error::new(format!("unknown table '{table}'")))?
            .insert(columns, rows)
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
}
