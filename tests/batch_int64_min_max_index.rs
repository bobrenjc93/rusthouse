use std::mem::size_of;

use rusthouse::batch::engine::{Database, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
use rusthouse::{
    Int64MinMaxBlockMetadata, Int64MinMaxIndexAdmission, Int64MinMaxIndexLimits,
    Int64MinMaxIndexRejection,
};

const SETUP: &str = "\
    CREATE TABLE events (id Int64, key Int64, selected Bool); \
    INSERT INTO events VALUES \
      (0, -9223372036854775808, false), (1, -9, true), (2, -8, false), (3, -7, true), \
      (4, 0, false), (5, 1, true), (6, 2, false), (7, 3, true), \
      (8, 100, false), (9, 101, true), (10, 102, false), \
      (11, 9223372036854775807, true);";

fn test_database(indexed: bool) -> Database {
    let mut database = Database::new();
    database.execute(SETUP).expect("setup succeeds");
    if indexed {
        assert!(matches!(
            database
                .create_int64_min_max_index(
                    "events",
                    "key",
                    Int64MinMaxIndexLimits::new(4, 3, usize::MAX),
                )
                .expect("valid index request"),
            Int64MinMaxIndexAdmission::Created(_)
        ));
    }
    database
}

fn query(database: &mut Database, sql: &str) -> StatementResult {
    database
        .execute(sql)
        .expect("query succeeds")
        .pop()
        .expect("one query result")
}

#[test]
fn indexed_and_unindexed_results_match_at_boundaries_order_pagination_and_having() {
    let mut indexed = test_database(true);
    let mut unindexed = test_database(false);
    let cases = [
        "SELECT id FROM events WHERE key = -9223372036854775808",
        "SELECT id FROM events WHERE key = -7",
        "SELECT id FROM events WHERE key = 0",
        "SELECT id FROM events WHERE key = 3",
        "SELECT id FROM events WHERE key = 100",
        "SELECT id FROM events WHERE key = 9223372036854775807",
        "SELECT id FROM events WHERE key = 99",
        "SELECT id FROM events WHERE key < -9223372036854775808",
        "SELECT id FROM events WHERE key <= -9223372036854775808",
        "SELECT id FROM events WHERE key > 9223372036854775807",
        "SELECT id FROM events WHERE key >= 9223372036854775807",
        "SELECT id FROM events WHERE 100 <= key",
        "SELECT id FROM events WHERE key != 0",
        "SELECT id FROM events WHERE key >= 0 AND selected = true",
        "SELECT id FROM events WHERE key >= 0 LIMIT 3 OFFSET 2",
        "SELECT id FROM events WHERE key >= 0 ORDER BY id DESC LIMIT 4 OFFSET 1",
        "SELECT DISTINCT selected FROM events WHERE key >= 0 LIMIT 1",
        "SELECT id, ROW_NUMBER() OVER () AS rn FROM events WHERE key >= 0 LIMIT 3",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id DESC) AS rn FROM events \
         WHERE key >= 0 LIMIT 3",
        "SELECT selected, COUNT(*) AS n FROM events WHERE key >= 100 \
         GROUP BY selected HAVING n >= 2 ORDER BY selected",
    ];

    for sql in cases {
        assert_eq!(
            query(&mut indexed, sql),
            query(&mut unindexed, sql),
            "{sql}"
        );
    }

    let indexed_metrics = indexed.index_pruning_metrics();
    assert!(indexed_metrics.scanned_blocks > 0);
    assert!(indexed_metrics.pruned_blocks > 0);
    assert_eq!(unindexed.index_pruning_metrics().scanned_blocks, 0);
    assert_eq!(unindexed.index_pruning_metrics().pruned_blocks, 0);

    let StatementResult::Query(system_metrics) =
        query(&mut indexed, "SELECT metric, value FROM system.metrics")
    else {
        panic!("system.metrics is a query")
    };
    assert!(system_metrics.rows.iter().any(|row| {
        row[0].as_display_string() == "rusthouse_index_pruned_blocks"
            && row[1].as_display_string() == indexed_metrics.pruned_blocks.to_string()
    }));
}

#[test]
fn between_prunes_disjoint_blocks_and_rechecks_overlaps_in_source_order() {
    let mut indexed = test_database(true);
    let mut unindexed = test_database(false);
    let cases = [
        (
            "SELECT id FROM events WHERE key BETWEEN 4 AND 99",
            Vec::new(),
            0,
            3,
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN -8 AND 1",
            vec![
                vec![Value::Int64(2)],
                vec![Value::Int64(3)],
                vec![Value::Int64(4)],
                vec![Value::Int64(5)],
            ],
            2,
            1,
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN 3 AND 3",
            vec![vec![Value::Int64(7)]],
            1,
            2,
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN 100 AND 0",
            Vec::new(),
            0,
            3,
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN -9223372036854775808 AND 9223372036854775807",
            (0..12).map(|id| vec![Value::Int64(id)]).collect(),
            3,
            0,
        ),
    ];

    for (sql, expected_rows, expected_scanned, expected_pruned) in cases {
        let before = indexed.index_pruning_metrics();
        let indexed_result = query(&mut indexed, sql);
        assert_eq!(indexed_result, query(&mut unindexed, sql), "{sql}");
        let StatementResult::Query(result) = indexed_result else {
            panic!("expected query result")
        };
        assert_eq!(result.rows, expected_rows, "{sql}");

        let after = indexed.index_pruning_metrics();
        assert_eq!(
            after.scanned_blocks - before.scanned_blocks,
            expected_scanned,
            "{sql}"
        );
        assert_eq!(
            after.pruned_blocks - before.pruned_blocks,
            expected_pruned,
            "{sql}"
        );
    }
    assert_eq!(unindexed.index_pruning_metrics().scanned_blocks, 0);
    assert_eq!(unindexed.index_pruning_metrics().pruned_blocks, 0);
}

#[test]
fn nullable_between_prunes_all_null_blocks_without_matching_nulls() {
    const NULLABLE_SETUP: &str = "\
        CREATE TABLE samples (key Nullable(Int64)); \
        INSERT INTO samples VALUES \
          (NULL), (NULL), (1), (NULL), (5), (7), (NULL), (10);";

    let mut indexed = Database::new();
    indexed.execute(NULLABLE_SETUP).unwrap();
    indexed
        .create_int64_min_max_index(
            "samples",
            "key",
            Int64MinMaxIndexLimits::new(2, 4, usize::MAX),
        )
        .unwrap();
    let mut unindexed = Database::new();
    unindexed.execute(NULLABLE_SETUP).unwrap();

    let sql = "SELECT key FROM samples WHERE key BETWEEN 5 AND 7";
    let result = query(&mut indexed, sql);
    assert_eq!(result, query(&mut unindexed, sql));
    let StatementResult::Query(result) = result else {
        panic!("expected query result")
    };
    assert_eq!(result.rows, [vec![Value::Int64(5)], vec![Value::Int64(7)]]);
    assert_eq!(indexed.index_pruning_metrics().scanned_blocks, 1);
    assert_eq!(indexed.index_pruning_metrics().pruned_blocks, 3);

    let sql =
        "SELECT key FROM samples WHERE key BETWEEN -9223372036854775808 AND 9223372036854775807";
    let result = query(&mut indexed, sql);
    assert_eq!(result, query(&mut unindexed, sql));
    let StatementResult::Query(result) = result else {
        panic!("expected query result")
    };
    assert_eq!(
        result.rows,
        [
            vec![Value::Int64(1)],
            vec![Value::Int64(5)],
            vec![Value::Int64(7)],
            vec![Value::Int64(10)],
        ]
    );
    assert_eq!(indexed.index_pruning_metrics().scanned_blocks, 4);
    assert_eq!(indexed.index_pruning_metrics().pruned_blocks, 4);
}

#[test]
fn all_null_between_prunes_every_block() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (key Nullable(Int64)); \
             INSERT INTO samples VALUES (NULL), (NULL), (NULL);",
        )
        .unwrap();
    database
        .create_int64_min_max_index(
            "samples",
            "key",
            Int64MinMaxIndexLimits::new(2, 2, usize::MAX),
        )
        .unwrap();

    let StatementResult::Query(result) = query(
        &mut database,
        "SELECT key FROM samples WHERE key BETWEEN -9223372036854775808 AND 9223372036854775807",
    ) else {
        panic!("expected query result")
    };
    assert!(result.rows.is_empty());
    assert_eq!(database.index_pruning_metrics().scanned_blocks, 0);
    assert_eq!(database.index_pruning_metrics().pruned_blocks, 2);
}

#[test]
fn unsupported_between_shapes_keep_the_full_scan_fallback() {
    let mut indexed = test_database(true);
    let mut unindexed = test_database(false);

    for sql in [
        "SELECT id FROM events WHERE key NOT BETWEEN 0 AND 100",
        "SELECT id FROM events WHERE key BETWEEN 0.0 AND 100.0",
        "SELECT id FROM events WHERE key BETWEEN 0 AND 100 AND selected = true",
        "SELECT id FROM events WHERE selected BETWEEN false AND true",
    ] {
        assert_eq!(
            query(&mut indexed, sql),
            query(&mut unindexed, sql),
            "{sql}"
        );
    }
    assert_eq!(indexed.index_pruning_metrics().scanned_blocks, 0);
    assert_eq!(indexed.index_pruning_metrics().pruned_blocks, 0);
}

#[test]
fn scan_row_limit_still_charges_the_complete_source_before_pruning() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 11,
        ..QueryResultLimits::default()
    });
    database.execute(SETUP).expect("setup is not a SELECT scan");
    database
        .create_int64_min_max_index(
            "events",
            "key",
            Int64MinMaxIndexLimits::new(4, 3, usize::MAX),
        )
        .expect("valid index request");

    assert_eq!(
        database.execute(
            "SELECT id FROM events \
             WHERE key BETWEEN 9223372036854775807 AND 9223372036854775807",
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT scanned rows",
            actual: 12,
            max: 11,
        })
    );
    assert_eq!(database.index_pruning_metrics().scanned_blocks, 0);
    assert_eq!(database.index_pruning_metrics().pruned_blocks, 0);
}

#[test]
fn admission_rejections_leave_exact_fallback_and_existing_slot_unchanged() {
    let mut database = test_database(false);
    let block_bytes = size_of::<Int64MinMaxBlockMetadata>();
    assert_eq!(
        database
            .create_int64_min_max_index(
                "events",
                "key",
                Int64MinMaxIndexLimits::new(0, 3, usize::MAX),
            )
            .unwrap(),
        Int64MinMaxIndexAdmission::Rejected(Int64MinMaxIndexRejection::ZeroBlockRows)
    );
    assert_eq!(
        database
            .create_int64_min_max_index(
                "events",
                "key",
                Int64MinMaxIndexLimits::new(4, 2, usize::MAX),
            )
            .unwrap(),
        Int64MinMaxIndexAdmission::Rejected(Int64MinMaxIndexRejection::BlockLimitExceeded {
            required: 3,
            max: 2,
        })
    );
    assert_eq!(
        database
            .create_int64_min_max_index(
                "events",
                "key",
                Int64MinMaxIndexLimits::new(4, 3, block_bytes * 3 - 1),
            )
            .unwrap(),
        Int64MinMaxIndexAdmission::Rejected(Int64MinMaxIndexRejection::ByteLimitExceeded {
            required: block_bytes * 3,
            max: block_bytes * 3 - 1,
        })
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM events WHERE key = 100"),
        query(
            &mut test_database(false),
            "SELECT id FROM events WHERE key = 100"
        )
    );
    assert_eq!(database.index_pruning_metrics().pruned_blocks, 0);

    database
        .create_int64_min_max_index(
            "events",
            "key",
            Int64MinMaxIndexLimits::new(4, 3, usize::MAX),
        )
        .unwrap();
    database
        .execute("CREATE TABLE other (value Int64); INSERT INTO other VALUES (1)")
        .unwrap();
    assert_eq!(
        database
            .create_int64_min_max_index(
                "other",
                "value",
                Int64MinMaxIndexLimits::new(1, 1, usize::MAX),
            )
            .unwrap(),
        Int64MinMaxIndexAdmission::Rejected(Int64MinMaxIndexRejection::SlotOccupied {
            table: "events".to_owned(),
        })
    );
    assert!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .int64_min_max_index_info()
            .is_some()
    );
}

#[test]
fn mutations_refresh_the_index_and_over_budget_or_schema_changes_invalidate_it() {
    let mut database = test_database(false);
    database
        .create_int64_min_max_index(
            "events",
            "key",
            Int64MinMaxIndexLimits::new(4, 4, usize::MAX),
        )
        .unwrap();

    database
        .execute(
            "ALTER TABLE events UPDATE key = 999 WHERE id = 0; \
             INSERT INTO events VALUES (12, 104, false); \
             DELETE FROM events WHERE id = 1;",
        )
        .unwrap();
    let info = database
        .catalog()
        .table("events")
        .unwrap()
        .int64_min_max_index_info()
        .expect("index remains admitted");
    assert_eq!(info.indexed_rows, 12);
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM events WHERE key BETWEEN 999 AND 999",
        ),
        query(
            &mut {
                let mut unindexed = test_database(false);
                unindexed
                    .execute(
                        "ALTER TABLE events UPDATE key = 999 WHERE id = 0; \
                         INSERT INTO events VALUES (12, 104, false); \
                         DELETE FROM events WHERE id = 1;",
                    )
                    .unwrap();
                unindexed
            },
            "SELECT id FROM events WHERE key BETWEEN 999 AND 999",
        )
    );

    database.execute("TRUNCATE TABLE events").unwrap();
    assert_eq!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .int64_min_max_index_info()
            .unwrap()
            .indexed_rows,
        0
    );
    database
        .execute("INSERT INTO events VALUES (20, 20, true)")
        .unwrap();
    assert_eq!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .int64_min_max_index_info()
            .unwrap()
            .indexed_rows,
        1
    );
    database
        .execute("ALTER TABLE events DROP COLUMN selected")
        .unwrap();
    assert!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .int64_min_max_index_info()
            .is_none()
    );

    let mut capped = Database::new();
    capped
        .execute("CREATE TABLE t (value Int64); INSERT INTO t VALUES (1), (2), (3), (4)")
        .unwrap();
    capped
        .create_int64_min_max_index("t", "value", Int64MinMaxIndexLimits::new(2, 2, usize::MAX))
        .unwrap();
    capped.execute("INSERT INTO t VALUES (5)").unwrap();
    assert!(
        capped
            .catalog()
            .table("t")
            .unwrap()
            .int64_min_max_index_info()
            .is_none()
    );
    assert_eq!(
        query(
            &mut capped,
            "SELECT value FROM t WHERE value BETWEEN 5 AND 5",
        ),
        query(
            &mut {
                let mut expected = Database::new();
                expected
                    .execute(
                        "CREATE TABLE t (value Int64); \
                         INSERT INTO t VALUES (1), (2), (3), (4), (5)",
                    )
                    .unwrap();
                expected
            },
            "SELECT value FROM t WHERE value BETWEEN 5 AND 5",
        )
    );
    assert_eq!(capped.index_pruning_metrics().scanned_blocks, 0);
    assert_eq!(capped.index_pruning_metrics().pruned_blocks, 0);
}
