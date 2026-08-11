use std::collections::{HashMap, HashSet};

use crate::batch::error::{Error, Result};
use crate::batch::storage::{
    ColumnDef, DEFAULT_MAX_ROWS_PER_TABLE, Table, TableLimits, validate_table_name,
};

/// An in-memory collection of named tables.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, Table>,
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
        self.tables.insert(key, table);
        Ok(())
    }

    /// Registers a completely constructed table using case-insensitive name
    /// resolution. An existing table is never replaced.
    pub(crate) fn register_table(&mut self, table: Table) -> Result<()> {
        let key = normalize(table.name());
        if self.tables.contains_key(&key) {
            return Err(Error::TableAlreadyExists(table.name().to_owned()));
        }
        self.tables.insert(key, table);
        Ok(())
    }

    /// Swaps one completely constructed table into an existing catalog slot.
    ///
    /// The caller is responsible for constructing the replacement with the
    /// existing table's display name. Lookup remains case-insensitive, and a
    /// missing target leaves the catalog unchanged.
    pub(crate) fn replace_table(&mut self, name: &str, replacement: Table) -> Result<Table> {
        let target = self
            .tables
            .get_mut(&normalize(name))
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))?;
        debug_assert_eq!(
            normalize(target.name()),
            normalize(replacement.name()),
            "a catalog replacement must retain the target name"
        );
        Ok(std::mem::replace(target, replacement))
    }

    /// Registers a completely constructed set of tables after preflighting
    /// every existing and intra-set case-insensitive name collision.
    ///
    /// The returned index identifies the first rejected table in input order.
    /// No table is inserted unless the complete set passes preflight.
    pub(crate) fn register_tables(
        &mut self,
        tables: Vec<Table>,
    ) -> std::result::Result<(), (usize, Error)> {
        let mut incoming_names = HashSet::with_capacity(tables.len());
        for (index, table) in tables.iter().enumerate() {
            let key = normalize(table.name());
            if self.tables.contains_key(&key) || !incoming_names.insert(key) {
                return Err((index, Error::TableAlreadyExists(table.name().to_owned())));
            }
        }

        for table in tables {
            self.tables.insert(normalize(table.name()), table);
        }
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
        self.tables.remove(&normalize(name)).is_some()
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

    /// Adds one nullable Int64 column using case-insensitive table resolution.
    pub fn add_nullable_int64_column(&mut self, table: &str, column: String) -> Result<()> {
        self.table_mut(table)?.add_nullable_int64_column(column)
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

    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables
            .get_mut(&normalize(name))
            .ok_or_else(|| Error::TableNotFound(name.to_owned()))
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

    /// Returns the display name of the table occupying the database-wide
    /// sparse-index slot, if any.
    pub(crate) fn int64_min_max_index_owner(&self) -> Option<&str> {
        self.tables
            .values()
            .find(|table| table.has_int64_min_max_index())
            .map(Table::name)
    }

    /// Returns the number of columns retained across all registered tables.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.tables
            .values()
            .map(|table| table.schema().len())
            .fold(0_usize, usize::saturating_add)
    }

    /// Returns the greatest one-based column position in any table.
    #[must_use]
    pub(crate) fn max_column_position(&self) -> usize {
        self.tables
            .values()
            .map(|table| table.schema().len())
            .max()
            .unwrap_or(0)
    }

    /// Returns the String payload bytes required by the exact
    /// `system.columns` metadata result without allocating.
    #[must_use]
    pub(crate) fn system_column_string_bytes(&self, database_name: &str) -> usize {
        self.tables
            .values()
            .map(|table| {
                debug_assert_eq!(table.schema().len(), table.columns().len());
                table
                    .schema()
                    .iter()
                    .zip(table.columns())
                    .map(|(column, values)| {
                        database_name
                            .len()
                            .saturating_add(table.name().len())
                            .saturating_add(column.name.len())
                            .saturating_add(values.metadata_type_name().len())
                    })
                    .fold(0_usize, usize::saturating_add)
            })
            .fold(0_usize, usize::saturating_add)
    }

    /// Returns the number of rows retained across all registered tables.
    #[must_use]
    pub fn retained_row_count(&self) -> usize {
        self.tables
            .values()
            .map(Table::row_count)
            .fold(0_usize, usize::saturating_add)
    }

    /// Returns scalar payload bytes retained across all tables without allocating.
    ///
    /// Per-table totals are maintained during mutations, so this visits tables
    /// but does not scan their values. The sum saturates at [`usize::MAX`] and
    /// excludes container capacity, schema text, and allocation metadata.
    #[must_use]
    pub fn retained_value_bytes(&self) -> usize {
        saturating_usize(
            self.tables
                .values()
                .map(Table::retained_value_bytes_exact)
                .fold(0_u128, u128::saturating_add),
        )
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

    /// Returns display names and row counts in deterministic,
    /// case-insensitive table-name order.
    #[must_use]
    pub(crate) fn table_row_counts(&self) -> Vec<(&str, usize)> {
        let mut tables = self.tables.iter().collect::<Vec<_>>();
        tables.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        tables
            .into_iter()
            .map(|(_, table)| (table.name(), table.row_count()))
            .collect()
    }

    /// Returns tables in deterministic, case-insensitive name order for
    /// bounded schema metadata materialization.
    #[must_use]
    pub(crate) fn tables_in_name_order(&self) -> Vec<&Table> {
        let mut tables = self.tables.iter().collect::<Vec<_>>();
        tables.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        tables.into_iter().map(|(_, table)| table).collect()
    }

    /// Returns owned display names, row counts, and cached retained-value byte
    /// counts in deterministic, case-insensitive table-name order.
    #[must_use]
    pub(crate) fn table_metrics(&self) -> Vec<(String, usize, usize)> {
        let mut tables = self.tables.iter().collect::<Vec<_>>();
        tables.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        tables
            .into_iter()
            .map(|(_, table)| {
                (
                    table.name().to_owned(),
                    table.row_count(),
                    table.retained_value_bytes(),
                )
            })
            .collect()
    }

    /// Returns the variable bytes needed to encode every table display name
    /// and decimal row and retained-value byte count without allocating or
    /// sorting.
    #[must_use]
    pub(crate) fn table_metric_variable_bytes(&self) -> (usize, usize, usize) {
        self.tables.values().fold(
            (0_usize, 0_usize, 0_usize),
            |(table_name_bytes, row_count_bytes, retained_value_byte_count_bytes), table| {
                (
                    table_name_bytes.saturating_add(table.name().len()),
                    row_count_bytes.saturating_add(usize_decimal_len(table.row_count())),
                    retained_value_byte_count_bytes
                        .saturating_add(usize_decimal_len(table.retained_value_bytes())),
                )
            },
        )
    }
}

fn saturating_usize(value: u128) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn normalize(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
}

fn usize_decimal_len(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
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
        assert_eq!(
            catalog.table_row_counts(),
            [("Alpha", 0), ("beta", 0), ("zebra", 0)]
        );
        assert_eq!(catalog.table_metric_variable_bytes(), (14, 3, 3));
        assert_eq!(
            catalog.table_metrics(),
            [
                ("Alpha".to_owned(), 0, 0),
                ("beta".to_owned(), 0, 0),
                ("zebra".to_owned(), 0, 0),
            ]
        );
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
            let numbers: &mut Table = catalog.table_mut("numbers").expect("numbers table");
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
