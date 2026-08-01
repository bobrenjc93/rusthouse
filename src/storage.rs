use std::collections::{HashMap, HashSet};

use crate::error::{Error, Result};
use crate::identifier::{Identifier, ObjectName};
use crate::value::{DataType, Value};

pub const MAX_COLUMNS: usize = 256;
pub const MAX_ROWS_PER_TABLE: usize = 5_000_000;
pub const MAX_TABLES: usize = 128;

/// A named, typed column in a table schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnSchema {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    /// Whether SQL name resolution treats the column name as quoted and case-sensitive.
    pub quoted: bool,
}

#[derive(Debug)]
enum ColumnData {
    Int64(Vec<Option<i64>>),
    Float64(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    String(Vec<Option<String>>),
}

impl ColumnData {
    fn new(kind: DataType) -> Self {
        match kind {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(column), Value::Int64(value)) => column.push(Some(value)),
            (Self::Float64(column), Value::Float64(value)) => column.push(Some(value)),
            (Self::Bool(column), Value::Bool(value)) => column.push(Some(value)),
            (Self::String(column), Value::String(value)) => column.push(Some(value)),
            (Self::Int64(column), Value::Null) => column.push(None),
            (Self::Float64(column), Value::Null) => column.push(None),
            (Self::Bool(column), Value::Null) => column.push(None),
            (Self::String(column), Value::Null) => column.push(None),
            _ => unreachable!("values are validated before reaching column storage"),
        }
    }

    fn get(&self, row: usize) -> Value {
        match self {
            Self::Int64(column) => column[row].map_or(Value::Null, Value::Int64),
            Self::Float64(column) => column[row].map_or(Value::Null, Value::Float64),
            Self::Bool(column) => column[row].map_or(Value::Null, Value::Bool),
            Self::String(column) => column[row]
                .as_ref()
                .map_or(Value::Null, |value| Value::String(value.clone())),
        }
    }
}

/// An in-memory table with one contiguous vector per schema column.
#[derive(Debug)]
pub struct Table {
    pub schema: Vec<ColumnSchema>,
    columns: Vec<ColumnData>,
    column_lookup: HashMap<String, usize>,
    row_count: usize,
}

impl Table {
    fn new(schema: Vec<ColumnSchema>) -> Result<Self> {
        if schema.is_empty() {
            return Err(Error::Catalog(
                "a table must have at least one column".to_owned(),
            ));
        }
        if schema.len() > MAX_COLUMNS {
            return Err(Error::Limit {
                resource: "columns per table",
                limit: MAX_COLUMNS,
            });
        }

        let mut column_lookup = HashMap::with_capacity(schema.len());
        for (index, column) in schema.iter().enumerate() {
            let key = Identifier {
                value: column.name.clone(),
                quoted: column.quoted,
            }
            .lookup_key();
            if column_lookup.insert(key, index).is_some() {
                return Err(Error::Catalog(format!(
                    "duplicate column '{}'",
                    column.name
                )));
            }
        }
        let columns = schema
            .iter()
            .map(|column| ColumnData::new(column.data_type))
            .collect();
        Ok(Self {
            schema,
            columns,
            column_lookup,
            row_count: 0,
        })
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub(crate) fn column_index(&self, name: &Identifier) -> Result<usize> {
        self.column_lookup
            .get(&name.lookup_key())
            .copied()
            .ok_or_else(|| Error::Execution(format!("unknown column '{}'", name.value)))
    }

    pub(crate) fn column_schema(&self, name: &Identifier) -> Result<&ColumnSchema> {
        Ok(&self.schema[self.column_index(name)?])
    }

    pub(crate) fn value(&self, column: usize, row: usize) -> Value {
        self.columns[column].get(row)
    }

    pub(crate) fn insert_rows(
        &mut self,
        names: Option<&[Identifier]>,
        rows: Vec<Vec<Value>>,
    ) -> Result<usize> {
        if self.row_count.saturating_add(rows.len()) > MAX_ROWS_PER_TABLE {
            return Err(Error::Limit {
                resource: "rows per table",
                limit: MAX_ROWS_PER_TABLE,
            });
        }

        let mapping = if let Some(names) = names {
            let mut seen = HashSet::with_capacity(names.len());
            let mut mapping = Vec::with_capacity(names.len());
            for name in names {
                let index = self.column_index(name)?;
                if !seen.insert(index) {
                    return Err(Error::Catalog(format!(
                        "column '{}' appears more than once in INSERT",
                        name.value
                    )));
                }
                mapping.push(index);
            }
            mapping
        } else {
            (0..self.schema.len()).collect()
        };

        let mut validated = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() != mapping.len() {
                return Err(Error::Catalog(format!(
                    "INSERT row has {} values but {} were expected",
                    row.len(),
                    mapping.len()
                )));
            }
            let mut full_row = vec![Value::Null; self.schema.len()];
            for (value, column_index) in row.into_iter().zip(mapping.iter().copied()) {
                let schema = &self.schema[column_index];
                let value = value.coerce(schema.data_type)?;
                if value == Value::Null && !schema.nullable {
                    return Err(Error::Type(format!(
                        "column '{}' is not nullable",
                        schema.name
                    )));
                }
                full_row[column_index] = value;
            }
            for (schema, value) in self.schema.iter().zip(&full_row) {
                if *value == Value::Null && !schema.nullable {
                    return Err(Error::Type(format!(
                        "INSERT omitted non-nullable column '{}'",
                        schema.name
                    )));
                }
            }
            validated.push(full_row);
        }

        let inserted = validated.len();
        for row in validated {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count += inserted;
        Ok(inserted)
    }
}

/// Owns the set of in-memory tables for a database session.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, Table>,
}

impl Catalog {
    pub(crate) fn create_table(
        &mut self,
        name: ObjectName,
        schema: Vec<ColumnSchema>,
        if_not_exists: bool,
    ) -> Result<()> {
        let key = name.lookup_key();
        if self.tables.contains_key(&key) {
            return if if_not_exists {
                Ok(())
            } else {
                Err(Error::Catalog(format!(
                    "table '{}' already exists",
                    name.display()
                )))
            };
        }
        if self.tables.len() >= MAX_TABLES {
            return Err(Error::Limit {
                resource: "tables",
                limit: MAX_TABLES,
            });
        }
        self.tables.insert(key, Table::new(schema)?);
        Ok(())
    }

    pub(crate) fn table(&self, name: &ObjectName) -> Result<&Table> {
        self.tables
            .get(&name.lookup_key())
            .ok_or_else(|| Error::Catalog(format!("unknown table '{}'", name.display())))
    }

    pub(crate) fn table_mut(&mut self, name: &ObjectName) -> Result<&mut Table> {
        self.tables
            .get_mut(&name.lookup_key())
            .ok_or_else(|| Error::Catalog(format!("unknown table '{}'", name.display())))
    }
}
