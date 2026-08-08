use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

use crate::batch::error::{Error, Result};
use crate::batch::storage::{
    ColumnDef, DEFAULT_MAX_ROWS_PER_TABLE, Table, TableLimits, validate_table_name,
};

/// An in-memory collection of named tables.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, Table>,
    column_count: u128,
    retained_row_count: u128,
    retained_value_bytes: u128,
}

#[derive(Debug, Clone, Copy)]
struct TableMeasurements {
    column_count: u128,
    retained_row_count: u128,
    retained_value_bytes: u128,
}

impl TableMeasurements {
    fn read(table: &Table) -> Self {
        Self {
            column_count: table.schema().len() as u128,
            retained_row_count: table.row_count() as u128,
            retained_value_bytes: table.retained_value_bytes_exact(),
        }
    }
}

/// Mutable access to one catalog table that reconciles cached metrics on drop.
///
/// This guard dereferences to [`Table`]. Keeping metric reconciliation in the
/// guard ensures every table mutation path updates the catalog's constant-time
/// totals, including direct callers of [`Catalog::table_mut`].
#[derive(Debug)]
pub struct TableMut<'a> {
    table: &'a mut Table,
    column_count: &'a mut u128,
    retained_row_count: &'a mut u128,
    retained_value_bytes: &'a mut u128,
    before: TableMeasurements,
}

impl Deref for TableMut<'_> {
    type Target = Table;

    fn deref(&self) -> &Self::Target {
        self.table
    }
}

impl DerefMut for TableMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.table
    }
}

impl Drop for TableMut<'_> {
    fn drop(&mut self) {
        let after = TableMeasurements::read(self.table);
        replace_measurement(
            self.column_count,
            self.before.column_count,
            after.column_count,
        );
        replace_measurement(
            self.retained_row_count,
            self.before.retained_row_count,
            after.retained_row_count,
        );
        replace_measurement(
            self.retained_value_bytes,
            self.before.retained_value_bytes,
            after.retained_value_bytes,
        );
    }
}

impl Catalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_table(&mut self, name: String, schema: Vec<ColumnDef>) -> Result<()> {
        self.create_table_with_row_cap(name, schema, DEFAULT_MAX_ROWS_PER_TABLE)
    }

    /// Creates a default-cap table unless its case-insensitive name exists.
    ///
    /// Returns `true` when a table was created and `false` when the existing
    /// table was retained unchanged.
    pub fn create_table_if_not_exists(
        &mut self,
        name: String,
        schema: Vec<ColumnDef>,
    ) -> Result<bool> {
        self.create_table_if_not_exists_with_row_cap(name, schema, DEFAULT_MAX_ROWS_PER_TABLE)
    }

    /// Creates a table unless its case-insensitive name is already registered.
    ///
    /// Returns `true` when a table was created and `false` when the existing
    /// table was retained unchanged.
    pub fn create_table_if_not_exists_with_row_cap(
        &mut self,
        name: String,
        schema: Vec<ColumnDef>,
        row_cap: usize,
    ) -> Result<bool> {
        self.create_table_if_not_exists_with_limits(
            name,
            schema,
            TableLimits {
                max_rows: row_cap,
                ..TableLimits::default()
            },
        )
    }

    /// Creates a table with explicit limits unless its name is already registered.
    pub fn create_table_if_not_exists_with_limits(
        &mut self,
        name: String,
        schema: Vec<ColumnDef>,
        limits: TableLimits,
    ) -> Result<bool> {
        if self.table_exists(&name) {
            return Ok(false);
        }
        self.create_table_with_limits(name, schema, limits)?;
        Ok(true)
    }

    /// Creates a table with an explicit maximum retained row count.
    pub fn create_table_with_row_cap(
        &mut self,
        name: String,
        schema: Vec<ColumnDef>,
        row_cap: usize,
    ) -> Result<()> {
        self.create_table_with_limits(
            name,
            schema,
            TableLimits {
                max_rows: row_cap,
                ..TableLimits::default()
            },
        )
    }

    /// Creates a table with explicit persistent resource limits.
    pub fn create_table_with_limits(
        &mut self,
        name: String,
        schema: Vec<ColumnDef>,
        limits: TableLimits,
    ) -> Result<()> {
        let key = normalize(&name);
        if self.tables.contains_key(&key) {
            return Err(Error::TableAlreadyExists(name));
        }
        let table = Table::with_limits(name, schema, limits)?;
        let measurements = TableMeasurements::read(&table);
        self.tables.insert(key, table);
        self.add_measurements(measurements);
        Ok(())
    }

    /// Removes one table using the catalog's case-insensitive name resolution.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        if self.drop_table_if_exists(name) {
            Ok(())
        } else {
            Err(Error::TableNotFound(name.to_owned()))
        }
    }

    /// Removes a table if its case-insensitive name exists.
    ///
    /// Returns `true` when a table was removed and `false` when the catalog was
    /// already missing that name.
    pub fn drop_table_if_exists(&mut self, name: &str) -> bool {
        let Some(table) = self.tables.remove(&normalize(name)) else {
            return false;
        };
        self.subtract_measurements(TableMeasurements::read(&table));
        true
    }

    /// Renames one table after validating the complete catalog change.
    ///
    /// Resolution is case-insensitive. A destination that resolves to a
    /// different table is rejected without changing either table. When both
    /// names resolve to the same key, only the table's display case changes.
    pub fn rename_table(&mut self, source: &str, destination: String) -> Result<()> {
        let source_key = normalize(source);
        let destination_key = normalize(&destination);

        if !self.tables.contains_key(&source_key) {
            return Err(Error::TableNotFound(source.to_owned()));
        }
        if source_key != destination_key && self.tables.contains_key(&destination_key) {
            return Err(Error::TableAlreadyExists(destination));
        }
        validate_table_name(&destination)?;

        if source_key == destination_key {
            self.tables
                .get_mut(&source_key)
                .expect("the rename source was preflighted")
                .set_name(destination);
            return Ok(());
        }

        let mut table = self
            .tables
            .remove(&source_key)
            .expect("the rename source was preflighted");
        table.set_name(destination);
        let replaced = self.tables.insert(destination_key, table);
        debug_assert!(replaced.is_none(), "the rename destination was preflighted");
        Ok(())
    }

    /// Renames one column using case-insensitive table and column resolution.
    ///
    /// The table validates the source and complete destination before changing
    /// only the column's stored display name.
    pub fn rename_column(&mut self, table: &str, source: &str, destination: String) -> Result<()> {
        self.table_mut(table)?.rename_column(source, destination)
    }

    /// Adds one typed column using case-insensitive table and column resolution.
    pub fn add_column(&mut self, table: &str, column: ColumnDef) -> Result<()> {
        self.table_mut(table)?.add_column(column)
    }

    /// Drops one column using case-insensitive table and column resolution.
    pub fn drop_column(&mut self, table: &str, column: &str) -> Result<()> {
        self.table_mut(table)?.drop_column(column)
    }

    pub fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(&normalize(name))
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
    }

    pub fn table_mut(&mut self, name: &str) -> Result<TableMut<'_>> {
        let key = normalize(name);
        let Self {
            tables,
            column_count,
            retained_row_count,
            retained_value_bytes,
        } = self;
        let table = tables
            .get_mut(&key)
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))?;
        let before = TableMeasurements::read(table);
        Ok(TableMut {
            table,
            column_count,
            retained_row_count,
            retained_value_bytes,
            before,
        })
    }

    /// Reports catalog membership using the same case-insensitive resolution
    /// as table access.
    #[must_use]
    pub fn table_exists(&self, name: &str) -> bool {
        self.tables.contains_key(&normalize(name))
    }

    /// Returns the number of registered tables without allocating.
    #[must_use]
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Returns the cached number of columns across all registered tables in
    /// constant time.
    #[must_use]
    pub fn column_count(&self) -> usize {
        saturating_usize(self.column_count)
    }

    /// Returns the cached number of rows across all registered tables in
    /// constant time.
    #[must_use]
    pub fn retained_row_count(&self) -> usize {
        saturating_usize(self.retained_row_count)
    }

    /// Returns cached scalar payload bytes across all tables in constant time.
    ///
    /// The total is maintained during mutations, saturates at [`usize::MAX`],
    /// and excludes container capacity, schema text, and allocation metadata.
    #[must_use]
    pub fn retained_value_bytes(&self) -> usize {
        saturating_usize(self.retained_value_bytes)
    }

    /// Returns the combined byte length of all display names without allocating.
    #[must_use]
    pub fn table_name_bytes(&self) -> usize {
        self.tables
            .values()
            .map(|table| table.name().len())
            .fold(0_usize, usize::saturating_add)
    }

    /// Returns display names in deterministic, case-insensitive order.
    #[must_use]
    pub fn table_names(&self) -> Vec<&str> {
        let mut tables = self.tables.iter().collect::<Vec<_>>();
        tables.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        tables.into_iter().map(|(_, table)| table.name()).collect()
    }

    fn add_measurements(&mut self, measurements: TableMeasurements) {
        self.column_count = self.column_count.saturating_add(measurements.column_count);
        self.retained_row_count = self
            .retained_row_count
            .saturating_add(measurements.retained_row_count);
        self.retained_value_bytes = self
            .retained_value_bytes
            .saturating_add(measurements.retained_value_bytes);
    }

    fn subtract_measurements(&mut self, measurements: TableMeasurements) {
        self.column_count = self.column_count.saturating_sub(measurements.column_count);
        self.retained_row_count = self
            .retained_row_count
            .saturating_sub(measurements.retained_row_count);
        self.retained_value_bytes = self
            .retained_value_bytes
            .saturating_sub(measurements.retained_value_bytes);
    }
}

fn replace_measurement(total: &mut u128, before: u128, after: u128) {
    *total = total.saturating_sub(before).saturating_add(after);
}

fn saturating_usize(value: u128) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn normalize(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::value::{DataType, Value};

    #[test]
    fn table_lookup_is_case_insensitive() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                "Events".to_owned(),
                vec![ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                }],
            )
            .expect("create table");

        assert_eq!(catalog.table("EVENTS").expect("lookup").name(), "Events");
    }

    #[test]
    fn table_names_are_sorted_without_changing_display_case() {
        let mut catalog = Catalog::new();
        assert_eq!(catalog.table_count(), 0);
        assert_eq!(catalog.column_count(), 0);
        assert_eq!(catalog.retained_row_count(), 0);
        assert_eq!(catalog.table_name_bytes(), 0);

        for name in ["zebra", "Alpha", "beta"] {
            catalog
                .create_table(
                    name.to_owned(),
                    vec![ColumnDef {
                        name: "id".to_owned(),
                        data_type: DataType::Int64,
                    }],
                )
                .expect("create table");
        }

        assert_eq!(catalog.table_count(), 3);
        assert_eq!(catalog.column_count(), 3);
        assert_eq!(catalog.retained_row_count(), 0);
        assert_eq!(catalog.table_name_bytes(), 14);
        assert_eq!(catalog.table_names(), ["Alpha", "beta", "zebra"]);
    }

    #[test]
    fn dropping_is_case_insensitive_and_a_missing_table_preserves_the_catalog() {
        let mut catalog = Catalog::new();
        for name in ["Events", "readings"] {
            catalog
                .create_table(
                    name.to_owned(),
                    vec![ColumnDef {
                        name: "id".to_owned(),
                        data_type: DataType::Int64,
                    }],
                )
                .expect("create table");
        }

        catalog.drop_table("EVENTS").expect("case-insensitive drop");
        assert_eq!(catalog.table_names(), ["readings"]);

        assert_eq!(
            catalog.drop_table("missing"),
            Err(Error::TableNotFound("missing".to_owned()))
        );
        assert_eq!(catalog.table_names(), ["readings"]);
        assert!(catalog.table("readings").is_ok());
    }

    #[test]
    fn conditional_drop_is_case_insensitive_and_missing_tables_are_no_ops() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                "Events".to_owned(),
                vec![ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                }],
            )
            .expect("create table");

        assert!(catalog.drop_table_if_exists("eVeNtS"));
        assert!(!catalog.drop_table_if_exists("EVENTS"));
        assert_eq!(catalog.table_count(), 0);
    }

    #[test]
    fn retained_value_bytes_aggregate_tables_and_drop_with_them() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                "numbers".to_owned(),
                vec![ColumnDef {
                    name: "value".to_owned(),
                    data_type: DataType::Int64,
                }],
            )
            .expect("create numbers");
        catalog
            .create_table(
                "labels".to_owned(),
                vec![ColumnDef {
                    name: "value".to_owned(),
                    data_type: DataType::String,
                }],
            )
            .expect("create labels");
        assert_eq!(catalog.column_count(), 2);
        assert_eq!(catalog.retained_row_count(), 0);
        assert_eq!(catalog.retained_value_bytes(), 0);

        {
            let mut numbers = catalog.table_mut("numbers").expect("numbers table");
            numbers
                .insert_row(vec![Value::Int64(7)])
                .expect("insert number");
        }
        catalog
            .table_mut("labels")
            .expect("labels table")
            .insert_row(vec![Value::String("é".to_owned())])
            .expect("insert label");

        assert_eq!(catalog.column_count(), 2);
        assert_eq!(catalog.retained_row_count(), 2);
        assert_eq!(catalog.retained_value_bytes(), 10);
        assert!(
            catalog
                .table_mut("numbers")
                .expect("numbers table")
                .insert_row(vec![Value::String("wrong".to_owned())])
                .is_err()
        );
        assert_eq!(catalog.column_count(), 2);
        assert_eq!(catalog.retained_row_count(), 2);
        assert_eq!(catalog.retained_value_bytes(), 10);

        catalog
            .add_column(
                "numbers",
                ColumnDef {
                    name: "active".to_owned(),
                    data_type: DataType::Bool,
                },
            )
            .expect("add defaulted bool column");
        assert_eq!(catalog.column_count(), 3);
        assert_eq!(catalog.retained_row_count(), 2);
        assert_eq!(catalog.retained_value_bytes(), 11);

        catalog
            .table_mut("labels")
            .expect("labels table")
            .delete_rows(&[0])
            .expect("delete label");
        assert_eq!(catalog.retained_row_count(), 1);
        assert_eq!(catalog.retained_value_bytes(), 9);

        catalog.drop_table("NUMBERS").expect("drop numbers");
        assert_eq!(catalog.column_count(), 1);
        assert_eq!(catalog.retained_row_count(), 0);
        assert_eq!(catalog.retained_value_bytes(), 0);
        catalog.drop_table("labels").expect("drop labels");
        assert_eq!(catalog.column_count(), 0);
        assert_eq!(catalog.retained_row_count(), 0);
        assert_eq!(catalog.retained_value_bytes(), 0);
    }
}
