use rusthouse::batch::engine::{QueryResult, QueryResultLimits, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{
    Database, Int64MinMaxIndexAdmission, Int64MinMaxIndexLimits, Int64RangePartition,
    Int64RangePartitionError, Int64RangePartitionLimits, TableLimits,
};

fn partitions() -> Vec<Int64RangePartition> {
    vec![
        Int64RangePartition::new(i64::MIN, -1, vec![i64::MIN, -1]),
        Int64RangePartition::new(0, 9, vec![0, 9]),
        Int64RangePartition::new(10, i64::MAX, vec![10, i64::MAX]),
    ]
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected exactly one query result");
    };
    result.clone()
}

fn one_int64_column(name: &str) -> Vec<ResultColumn> {
    vec![ResultColumn {
        name: name.to_owned(),
        data_type: DataType::Int64,
    }]
}

#[test]
fn selects_across_partitions_with_boundaries_alias_order_and_pagination() {
    let mut database = Database::new();
    database
        .create_int64_range_partitioned_table("Events", "id", partitions())
        .expect("partitioned table is valid");

    assert_eq!(
        query(
            &mut database,
            "SELECT id AS boundary FROM events WHERE id >= 0 \
             ORDER BY boundary DESC LIMIT 3 OFFSET 1",
        ),
        QueryResult {
            columns: one_int64_column("boundary"),
            rows: vec![
                vec![Value::Int64(10)],
                vec![Value::Int64(9)],
                vec![Value::Int64(0)],
            ],
        }
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM events WHERE id >= -1").rows,
        vec![
            vec![Value::Int64(-1)],
            vec![Value::Int64(0)],
            vec![Value::Int64(9)],
            vec![Value::Int64(10)],
            vec![Value::Int64(i64::MAX)],
        ]
    );

    for (sql, expected) in [
        (
            "SELECT id FROM events WHERE id = -9223372036854775808",
            i64::MIN,
        ),
        (
            "SELECT id FROM events WHERE id <= -9223372036854775808",
            i64::MIN,
        ),
        (
            "SELECT id FROM events WHERE id >= 9223372036854775807",
            i64::MAX,
        ),
        ("SELECT id FROM events WHERE 10 <= id LIMIT 1", 10),
    ] {
        assert_eq!(
            query(&mut database, sql).rows,
            [vec![Value::Int64(expected)]]
        );
    }

    assert_eq!(
        query(
            &mut database,
            "SELECT MIN(id) AS minimum FROM events WHERE id > 9223372036854775807",
        ),
        QueryResult {
            columns: one_int64_column("minimum"),
            rows: vec![vec![Value::Null(DataType::Int64)]],
        }
    );
    assert_eq!(
        database.execute("SELECT id FROM events WHERE id = NULL"),
        Err(Error::ColumnNotFound {
            table: "Events".to_owned(),
            column: "NULL".to_owned(),
        })
    );
}

#[test]
fn pruning_is_bounded_and_unsupported_or_unpartitioned_predicates_fall_back() {
    let limits = QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .create_int64_range_partitioned_table("events", "id", partitions())
        .expect("partitioned table is valid");

    assert_eq!(
        query(&mut database, "SELECT id FROM events WHERE id = 10").rows,
        [vec![Value::Int64(10)]]
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM events WHERE id < 0").rows,
        [vec![Value::Int64(i64::MIN)], vec![Value::Int64(-1)]]
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM events WHERE id >= 10").rows,
        [vec![Value::Int64(10)], vec![Value::Int64(i64::MAX)]]
    );
    assert_eq!(
        database.execute("SELECT id FROM events WHERE id != 10"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT scanned rows",
            actual: 6,
            max: 2,
        })
    );

    let mut ordinary = Database::with_query_result_limits(limits);
    ordinary
        .execute(
            "CREATE TABLE events (id Int64); \
             INSERT INTO events VALUES (-2), (-1), (0), (1), (2), (3);",
        )
        .expect("ordinary table setup");
    assert_eq!(
        ordinary.execute("SELECT id FROM events WHERE id = 1"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT scanned rows",
            actual: 6,
            max: 2,
        })
    );
}

#[test]
fn range_partitions_and_sparse_index_compose_without_changing_scan_charges() {
    let limits = QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .create_int64_range_partitioned_table("events", "id", partitions())
        .expect("partitioned table is valid");
    assert!(matches!(
        database
            .create_int64_min_max_index(
                "events",
                "id",
                Int64MinMaxIndexLimits::new(1, 7, usize::MAX),
            )
            .expect("sparse index is valid"),
        Int64MinMaxIndexAdmission::Created(_)
    ));
    assert_eq!(
        database
            .catalog()
            .table("events")
            .expect("table exists")
            .int64_range_partition_count(),
        Some(3)
    );

    assert_eq!(
        query(&mut database, "SELECT id FROM events WHERE id = 10").rows,
        [vec![Value::Int64(10)]]
    );
    let pruning = database.index_pruning_metrics();
    assert_eq!(pruning.scanned_blocks, 1);
    assert_eq!(pruning.pruned_blocks, 1);

    database
        .execute("INSERT INTO events VALUES (7)")
        .expect("successful mutation");
    let table = database.catalog().table("events").expect("table remains");
    assert_eq!(table.int64_range_partition_count(), None);
    assert_eq!(
        table
            .int64_min_max_index_info()
            .expect("sparse index was refreshed")
            .indexed_rows,
        7
    );
    assert_eq!(
        database.execute("SELECT id FROM events WHERE id = 10"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT scanned rows",
            actual: 7,
            max: 2,
        })
    );
    assert_eq!(database.index_pruning_metrics(), pruning);
}

#[test]
fn nullable_add_column_invalidates_range_metadata_and_backfills_partitioned_rows() {
    let mut database = Database::new();
    database
        .create_int64_range_partitioned_table("Events", "id", partitions())
        .expect("partitioned table is valid");
    database
        .create_int64_min_max_index(
            "events",
            "id",
            Int64MinMaxIndexLimits::new(2, 3, usize::MAX),
        )
        .expect("sparse index is valid");

    database
        .execute("ALTER TABLE Events ADD COLUMN IF NOT EXISTS measurement Nullable(Int64)")
        .expect("nullable schema evolution succeeds");

    let table = database.catalog().table("events").unwrap();
    assert_eq!(table.int64_range_partition_count(), None);
    assert_eq!(
        table
            .int64_min_max_index_info()
            .expect("the unaffected sparse index is refreshed")
            .indexed_rows,
        6
    );
    assert!(matches!(
        &table.columns()[1],
        rusthouse::batch::storage::Column::NullableInt64(values)
            if values == &[None, None, None, None, None, None]
    ));
}

#[test]
fn invalid_layouts_and_construction_limits_are_typed_and_atomic() {
    let limits = Int64RangePartitionLimits::new(4, 4, 32);
    let cases = [
        (
            vec![Int64RangePartition::new(2, 1, vec![])],
            Int64RangePartitionError::InvalidRange {
                partition_index: 0,
                lower_bound: 2,
                upper_bound: 1,
            },
        ),
        (
            vec![
                Int64RangePartition::new(10, 20, vec![10]),
                Int64RangePartition::new(-10, -1, vec![-1]),
            ],
            Int64RangePartitionError::OutOfOrder {
                partition_index: 1,
                previous_lower_bound: 10,
                lower_bound: -10,
            },
        ),
        (
            vec![
                Int64RangePartition::new(0, 10, vec![0]),
                Int64RangePartition::new(10, 20, vec![20]),
            ],
            Int64RangePartitionError::Overlap {
                partition_index: 1,
                previous_upper_bound: 10,
                lower_bound: 10,
            },
        ),
        (
            vec![Int64RangePartition::new(0, 10, vec![11])],
            Int64RangePartitionError::ValueOutOfRange {
                partition_index: 0,
                value_index: 0,
                value: 11,
                lower_bound: 0,
                upper_bound: 10,
            },
        ),
    ];
    for (partitions, expected) in cases {
        let mut database = Database::new();
        assert_eq!(
            database.create_int64_range_partitioned_table_with_limits(
                "events", "id", partitions, limits,
            ),
            Err(expected)
        );
        assert_eq!(database.catalog().table_count(), 0);
    }

    for (limits, expected) in [
        (
            Int64RangePartitionLimits::new(0, 1, 8),
            Int64RangePartitionError::PartitionLimitExceeded {
                partitions: 1,
                max_partitions: 0,
            },
        ),
        (
            Int64RangePartitionLimits::new(1, 0, 8),
            Int64RangePartitionError::RowLimitExceeded {
                rows: 1,
                max_rows: 0,
            },
        ),
        (
            Int64RangePartitionLimits::new(1, 1, 7),
            Int64RangePartitionError::ByteLimitExceeded {
                bytes: 8,
                max_bytes: 7,
            },
        ),
    ] {
        let mut database = Database::new();
        assert_eq!(
            database.create_int64_range_partitioned_table_with_limits(
                "events",
                "id",
                vec![Int64RangePartition::new(0, 0, vec![0])],
                limits,
            ),
            Err(expected)
        );
        assert_eq!(database.catalog().table_count(), 0);
    }

    let mut table_limited = Database::with_table_limits(TableLimits::new(1, 1, 1));
    assert_eq!(
        table_limited.create_int64_range_partitioned_table_with_limits(
            "events",
            "id",
            vec![Int64RangePartition::new(0, 1, vec![0, 1])],
            Int64RangePartitionLimits::new(1, 2, 16),
        ),
        Err(Int64RangePartitionError::Table(
            Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 2,
                max: 1,
            }
        ))
    );
    assert_eq!(table_limited.catalog().table_count(), 0);

    let mut duplicate = Database::new();
    duplicate
        .create_int64_range_partitioned_table("events", "id", partitions())
        .expect("first table");
    assert_eq!(
        duplicate.create_int64_range_partitioned_table(
            "EVENTS",
            "replacement",
            vec![Int64RangePartition::new(0, 0, vec![0])],
        ),
        Err(Int64RangePartitionError::Table(Error::TableAlreadyExists(
            "EVENTS".to_owned()
        )))
    );
    let original = duplicate.catalog().table("events").expect("original table");
    assert_eq!(original.schema()[0].name.as_str(), "id");
    assert_eq!(original.row_count(), 6);
    assert_eq!(original.int64_range_partition_count(), Some(3));
}

#[test]
fn mutation_invalidates_pruning_and_catalog_metrics_count_one_physical_table() {
    let limits = QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .create_int64_range_partitioned_table("events", "id", partitions())
        .expect("partitioned table is valid");

    let table = database.catalog().table("EVENTS").expect("published table");
    assert_eq!(table.int64_range_partition_count(), Some(3));
    assert_eq!(table.row_count(), 6);
    assert_eq!(table.retained_value_bytes(), 48);
    assert_eq!(
        query(&mut database, "SELECT metric, value FROM system.metrics").rows,
        vec![
            vec![
                Value::String("rusthouse_tables".to_owned()),
                Value::Int64(1)
            ],
            vec![
                Value::String("rusthouse_columns".to_owned()),
                Value::Int64(1)
            ],
            vec![
                Value::String("rusthouse_retained_rows".to_owned()),
                Value::Int64(6),
            ],
            vec![
                Value::String("rusthouse_retained_value_bytes".to_owned()),
                Value::Int64(48),
            ],
            vec![
                Value::String("rusthouse_index_scanned_blocks".to_owned()),
                Value::Int64(0),
            ],
            vec![
                Value::String("rusthouse_index_pruned_blocks".to_owned()),
                Value::Int64(0),
            ],
        ]
    );

    database
        .execute("INSERT INTO events VALUES (7)")
        .expect("successful mutation");
    let table = database.catalog().table("events").expect("table remains");
    assert_eq!(table.int64_range_partition_count(), None);
    assert_eq!(table.row_count(), 7);
    assert_eq!(table.retained_value_bytes(), 56);
    let metrics = query(&mut database, "SELECT metric, value FROM system.metrics");
    assert_eq!(metrics.rows[2][1], Value::Int64(7));
    assert_eq!(metrics.rows[3][1], Value::Int64(56));
    assert_eq!(
        database.execute("SELECT id FROM events WHERE id = 10"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT scanned rows",
            actual: 7,
            max: 2,
        })
    );
}
