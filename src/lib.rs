//! RustHouse is an experimental, compact analytical database.

pub mod catalog;

pub use catalog::{
    CatalogImage, ColumnData, ColumnImage, Corruption, DataType, SNAPSHOT_FORMAT_VERSION,
    SNAPSHOT_MAGIC, SchemaImage, SnapshotError, SnapshotLimits, SnapshotStore, TableImage,
};

/// Returns the product name while the first storage engine is being built.
pub fn product_name() -> &'static str {
    "RustHouse"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_database() {
        assert_eq!(product_name(), "RustHouse");
    }
}
