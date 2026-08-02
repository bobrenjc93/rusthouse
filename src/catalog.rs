//! A bounded, in-memory catalog for parsed table definitions.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::parser::{CreateTable, MAX_COLUMNS, MAX_INPUT_BYTES};
use crate::storage::{ColumnSchema, DataType, Schema, SchemaError, Table};

/// Configurable resource limits for an in-memory [`Catalog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogLimits {
    pub max_tables: usize,
}

impl CatalogLimits {
    pub const DEFAULT_MAX_TABLES: usize = 1024;

    pub const fn new(max_tables: usize) -> Self {
        Self { max_tables }
    }
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_TABLES)
    }
}

/// A table definition rejected without changing the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    DuplicateTable {
        name: String,
    },
    TableLimitExceeded {
        limit: usize,
    },
    TooManyColumns {
        limit: usize,
        actual: usize,
    },
    DefinitionTooLong {
        limit: usize,
        actual: usize,
    },
    InvalidTableName {
        reason: IdentifierError,
    },
    InvalidColumnName {
        column: usize,
        reason: IdentifierError,
    },
    DuplicateColumn {
        name: String,
        first_column: usize,
        duplicate_column: usize,
    },
    InvalidSchema(SchemaError),
}

/// The parser identifier rule violated by a manually constructed syntax tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierError {
    Empty,
    InvalidStart { character: char },
    InvalidCharacter { character: char, position: usize },
}

impl fmt::Display for IdentifierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("identifier is empty"),
            Self::InvalidStart { character } => {
                write!(
                    formatter,
                    "identifier starts with invalid character {character:?}"
                )
            }
            Self::InvalidCharacter {
                character,
                position,
            } => write!(
                formatter,
                "identifier contains invalid character {character:?} at byte {position}"
            ),
        }
    }
}

impl Error for IdentifierError {}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTable { name } => write!(formatter, "table {name:?} already exists"),
            Self::TableLimitExceeded { limit } => {
                write!(
                    formatter,
                    "catalog table limit of {limit} would be exceeded"
                )
            }
            Self::TooManyColumns { limit, actual } => write!(
                formatter,
                "table has {actual} columns, exceeding the limit of {limit}"
            ),
            Self::DefinitionTooLong { limit, actual } => write!(
                formatter,
                "table definition requires at least {actual} SQL bytes, exceeding the limit of {limit}"
            ),
            Self::InvalidTableName { reason } => write!(formatter, "invalid table name: {reason}"),
            Self::InvalidColumnName { column, reason } => {
                write!(formatter, "invalid name for column {column}: {reason}")
            }
            Self::DuplicateColumn {
                name,
                first_column,
                duplicate_column,
            } => write!(
                formatter,
                "duplicate column {name:?} at index {duplicate_column}; first defined at index {first_column}"
            ),
            Self::InvalidSchema(error) => write!(formatter, "invalid table schema: {error}"),
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSchema(error) => Some(error),
            Self::InvalidTableName { reason } | Self::InvalidColumnName { reason, .. } => {
                Some(reason)
            }
            Self::DuplicateTable { .. }
            | Self::TableLimitExceeded { .. }
            | Self::TooManyColumns { .. }
            | Self::DefinitionTooLong { .. }
            | Self::DuplicateColumn { .. } => None,
        }
    }
}

/// A bounded collection of named, in-memory tables.
///
/// Table names use the parser's ASCII case-insensitive identifier semantics.
/// The spelling from the `CREATE TABLE` statement is retained by [`table_name`](Self::table_name).
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    tables: HashMap<String, CatalogTable>,
    limits: CatalogLimits,
}

#[derive(Debug, Clone, PartialEq)]
struct CatalogTable {
    name: String,
    table: Table,
}

impl Catalog {
    /// Creates an empty catalog with default resource limits.
    pub fn new() -> Self {
        Self::with_limits(CatalogLimits::default())
    }

    /// Creates an empty catalog with the supplied resource limits.
    pub fn with_limits(limits: CatalogLimits) -> Self {
        Self {
            tables: HashMap::new(),
            limits,
        }
    }

    pub fn limits(&self) -> CatalogLimits {
        self.limits
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Creates an empty table from a parsed statement.
    ///
    /// SQL column definitions are currently non-nullable, so the generated
    /// schema does not allocate NULL bitmaps. Duplicate and lookup comparisons
    /// are ASCII case-insensitive.
    pub fn create_table(&mut self, statement: CreateTable) -> Result<(), CatalogError> {
        validate_statement(&statement)?;

        let normalized_name = normalize_name(&statement.name);
        if self.tables.contains_key(&normalized_name) {
            return Err(CatalogError::DuplicateTable {
                name: statement.name,
            });
        }
        if self.tables.len() == self.limits.max_tables {
            return Err(CatalogError::TableLimitExceeded {
                limit: self.limits.max_tables,
            });
        }

        let mut columns = Vec::with_capacity(statement.columns.len());
        for column in &statement.columns {
            columns.push(ColumnSchema::new(
                column.name.as_str().to_owned(),
                column.column_type,
                false,
            ));
        }
        let schema = Schema::new(columns).map_err(CatalogError::InvalidSchema)?;

        self.tables.insert(
            normalized_name,
            CatalogTable {
                name: statement.name.as_str().to_owned(),
                table: Table::new(schema),
            },
        );
        Ok(())
    }

    /// Looks up a table using an ASCII case-insensitive name.
    pub fn table(&self, name: &str) -> Option<&Table> {
        let normalized_name = normalize_lookup_name(name)?;
        self.tables.get(&normalized_name).map(|entry| &entry.table)
    }

    /// Returns the spelling used when a table was created.
    pub fn table_name(&self, name: &str) -> Option<&str> {
        let normalized_name = normalize_lookup_name(name)?;
        self.tables
            .get(&normalized_name)
            .map(|entry| entry.name.as_str())
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn normalize_lookup_name(name: &str) -> Option<String> {
    if name.len() > MAX_INPUT_BYTES {
        return None;
    }
    Some(normalize_name(name))
}

fn validate_statement(statement: &CreateTable) -> Result<(), CatalogError> {
    if statement.columns.is_empty() {
        return Err(CatalogError::InvalidSchema(SchemaError::Empty));
    }
    if statement.columns.len() > MAX_COLUMNS {
        return Err(CatalogError::TooManyColumns {
            limit: MAX_COLUMNS,
            actual: statement.columns.len(),
        });
    }

    let definition_size = minimum_sql_size(statement);
    if definition_size > MAX_INPUT_BYTES {
        return Err(CatalogError::DefinitionTooLong {
            limit: MAX_INPUT_BYTES,
            actual: definition_size,
        });
    }

    validate_identifier(&statement.name)
        .map_err(|reason| CatalogError::InvalidTableName { reason })?;

    let mut column_names = HashMap::with_capacity(statement.columns.len());
    for (column, definition) in statement.columns.iter().enumerate() {
        validate_identifier(&definition.name)
            .map_err(|reason| CatalogError::InvalidColumnName { column, reason })?;

        let normalized_name = normalize_name(&definition.name);
        if let Some(first_column) = column_names.insert(normalized_name, column) {
            return Err(CatalogError::DuplicateColumn {
                name: definition.name.clone(),
                first_column,
                duplicate_column: column,
            });
        }
    }

    Ok(())
}

fn minimum_sql_size(statement: &CreateTable) -> usize {
    let mut size = "CREATE TABLE ".len();
    size = size.saturating_add(statement.name.len());
    size = size.saturating_add(2); // Parentheses.

    for (index, column) in statement.columns.iter().enumerate() {
        if index != 0 {
            size = size.saturating_add(1); // Comma.
        }
        size = size.saturating_add(column.name.len());
        size = size.saturating_add(1); // Required separator before the type.
        size = size.saturating_add(type_name_len(column.column_type));
    }

    size
}

fn type_name_len(data_type: DataType) -> usize {
    match data_type {
        DataType::Int64 => "Int64".len(),
        DataType::Float64 => "Float64".len(),
        DataType::Bool => "Bool".len(),
        DataType::String => "String".len(),
    }
}

fn validate_identifier(name: &str) -> Result<(), IdentifierError> {
    let mut characters = name.char_indices();
    let Some((_, first)) = characters.next() else {
        return Err(IdentifierError::Empty);
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(IdentifierError::InvalidStart { character: first });
    }

    for (position, character) in characters {
        if !character.is_ascii_alphanumeric() && character != '_' {
            return Err(IdentifierError::InvalidCharacter {
                character,
                position,
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ColumnDefinition;

    #[test]
    fn accepted_ast_does_not_retain_forged_spare_capacity() {
        let string_capacity = MAX_INPUT_BYTES * 4;
        let mut table_name = String::with_capacity(string_capacity);
        table_name.push_str("events");
        let mut column_name = String::with_capacity(string_capacity);
        column_name.push_str("id");

        let forged_column_capacity = MAX_COLUMNS * 4;
        let mut columns = Vec::with_capacity(forged_column_capacity);
        columns.push(ColumnDefinition {
            name: column_name,
            column_type: DataType::Int64,
        });
        assert!(table_name.capacity() > MAX_INPUT_BYTES);
        assert!(columns.capacity() > MAX_COLUMNS);
        assert!(columns[0].name.capacity() > MAX_INPUT_BYTES);

        let mut catalog = Catalog::new();
        catalog
            .create_table(CreateTable {
                name: table_name,
                columns,
            })
            .unwrap();

        let entry = catalog.tables.get("events").unwrap();
        assert!(entry.name.capacity() <= MAX_INPUT_BYTES);
        assert!(entry.table.schema().column_capacity() <= MAX_COLUMNS);
        assert!(entry.table.schema().column_name_capacity(0).unwrap() <= MAX_INPUT_BYTES);
    }
}
