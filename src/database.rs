use crate::{Catalog, Error, MAX_BATCH_ROWS, Result, Table, TableSchema, sql};

/// Default maximum size of one SQL statement, in UTF-8 bytes.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Default maximum number of columns in one table.
pub const DEFAULT_MAX_COLUMNS_PER_TABLE: usize = 1024;

/// Resource limits applied before a statement changes the catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseConfig {
    pub max_input_bytes: usize,
    pub max_columns_per_table: usize,
}

impl DatabaseConfig {
    #[must_use]
    pub const fn new(max_input_bytes: usize, max_columns_per_table: usize) -> Self {
        Self {
            max_input_bytes,
            max_columns_per_table,
        }
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_INPUT_BYTES, DEFAULT_MAX_COLUMNS_PER_TABLE)
    }
}

/// A reusable in-memory SQL database.
#[derive(Debug)]
pub struct Database {
    catalog: Catalog,
    config: DatabaseConfig,
}

impl Database {
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(DatabaseConfig::default())
    }

    #[must_use]
    pub fn with_config(config: DatabaseConfig) -> Self {
        Self {
            catalog: Catalog::new(),
            config,
        }
    }

    #[must_use]
    pub fn config(&self) -> DatabaseConfig {
        self.config
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Find a table's typed columnar data using a case-insensitive name.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.catalog.table_data(name)
    }

    /// Parse and execute exactly one supported SQL statement.
    ///
    /// Insertion batches are bounded and fully validated before any row is
    /// committed to typed storage.
    pub fn execute(&mut self, input: &str) -> Result<()> {
        let input_bytes = input.len();
        if input_bytes > self.config.max_input_bytes {
            return Err(Error::InputTooLarge {
                actual: input_bytes,
                maximum: self.config.max_input_bytes,
            });
        }

        let statement =
            sql::parse_statement(input, self.config.max_columns_per_table, MAX_BATCH_ROWS)?;
        match statement {
            sql::Statement::CreateTable(statement) => {
                let (name, columns) = statement.into_parts();
                let schema = TableSchema::new(name, columns)?;
                self.catalog.register(schema)
            }
            sql::Statement::Insert(statement) => {
                let (table_name, rows) = statement.into_parts();
                self.catalog.insert_rows(&table_name, rows)
            }
        }
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}
