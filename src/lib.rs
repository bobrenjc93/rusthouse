//! RustHouse is a compact analytical database with snapshot-isolated sessions.

pub mod batch;
pub mod catalog;
mod database;
pub mod error;
pub mod formats;
pub mod http;
pub mod kernels;
mod persistence;
pub mod query;
mod sql;
pub mod storage;

pub use catalog::{
    CatalogImage, ColumnData, ColumnImage, Corruption, SNAPSHOT_FORMAT_VERSION, SNAPSHOT_MAGIC,
    SchemaImage, SnapshotError, SnapshotLimits, SnapshotStore, TableImage,
};
pub use database::{Database, ResultSet, Session, StatementResult, TransactionLimits};
pub use error::{Error, LimitKind, Result};
pub use query::{
    QueryCancellation, QueryError, QueryErrorKind, QueryFuture, QueryRequest, QueryResult,
    QueryService, QueryValue, ServiceHealth,
};
pub use storage::{Column, ColumnBatch, ColumnDef, DataType, Field, Schema, Table, Value};

/// Returns the product name.
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
