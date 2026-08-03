//! RustHouse is an experimental, compact analytical database.

pub mod aggregate;
pub mod csv;
pub mod grouping;
pub mod order;
pub mod parser;
pub mod scan;
pub mod snapshot;

pub use aggregate::{
    AggregateError, AggregateLimits, NullableI64Aggregates, RowSelection, aggregate_nullable_i64,
};
pub use csv::{CsvIngestError, CsvIngestLimits, ingest_csv_with_names};
pub use grouping::{
    GroupedCountError, GroupedCountLimits, NullableI64GroupedCount, grouped_count_nullable_i64,
};
pub use order::{NullOrder, OrderDirection, OrderError, OrderLimits, order_nullable_i64};
pub use parser::{
    ColumnDefinition, CreateTableStatement, Identifier, InsertStatement, ParseError, ParseLimits,
    parse_create_table, parse_insert,
};
pub use scan::{
    ComparisonOperator, NullPredicate, ScanError, ScanLimits, scan_nullable_i64,
    scan_nullable_i64_nullness,
};
pub use snapshot::{SnapshotCodec, SnapshotError};
mod storage;

pub use storage::{ColumnSchema, DataType, InsertError, Int64Table, Schema};

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
