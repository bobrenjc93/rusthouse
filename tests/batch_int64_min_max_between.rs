use rusthouse::batch::engine::{Database, QueryResult, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;
use rusthouse::{IndexPruningMetrics, Int64MinMaxIndexAdmission, Int64MinMaxIndexLimits};

const INT64_SETUP: &str = "\
    CREATE TABLE events (id Int64, key Int64, selected Bool); \
    INSERT INTO events VALUES \
      (0, -9223372036854775808, false), (1, -9, true), \
      (2, -8, false), (3, -7, true), \
      (4, 0, false), (5, 1, true), (6, 2, false), (7, 3, true), \
      (8, 100, false), (9, 101, true), (10, 102, false), \
      (11, 9223372036854775807, true);";

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    result.clone()
}

fn int64_database(indexed: bool) -> Database {
    let mut database = Database::new();
    database.execute(INT64_SETUP).expect("setup succeeds");
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

fn rows(values: &[i64]) -> Vec<Vec<Value>> {
    values
        .iter()
        .map(|value| vec![Value::Int64(*value)])
        .collect()
}

fn assert_indexed_range(
    indexed: &mut Database,
    unindexed: &mut Database,
    sql: &str,
    expected: &[i64],
    expected_delta: IndexPruningMetrics,
) {
    let before = indexed.index_pruning_metrics();
    let indexed_result = query(indexed, sql);
    assert_eq!(
        indexed_result.rows,
        rows(expected),
        "indexed result for {sql}"
    );
    assert_eq!(indexed_result, query(unindexed, sql), "fallback for {sql}");

    let after = indexed.index_pruning_metrics();
    assert_eq!(
        after.scanned_blocks - before.scanned_blocks,
        expected_delta.scanned_blocks,
        "scanned blocks for {sql}",
    );
    assert_eq!(
        after.pruned_blocks - before.pruned_blocks,
        expected_delta.pruned_blocks,
        "pruned blocks for {sql}",
    );
    assert_eq!(
        unindexed.index_pruning_metrics(),
        IndexPruningMetrics::default(),
    );
}

#[test]
fn int64_between_prunes_disjoint_blocks_and_rechecks_overlaps_in_source_order() {
    let mut indexed = int64_database(true);
    let mut unindexed = int64_database(false);

    for (sql, expected, expected_delta) in [
        (
            "SELECT id FROM events WHERE key BETWEEN 4 AND 99",
            vec![],
            IndexPruningMetrics {
                scanned_blocks: 0,
                pruned_blocks: 3,
            },
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN -8 AND 1",
            vec![2, 3, 4, 5],
            IndexPruningMetrics {
                scanned_blocks: 2,
                pruned_blocks: 1,
            },
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN 1 AND 1",
            vec![5],
            IndexPruningMetrics {
                scanned_blocks: 1,
                pruned_blocks: 2,
            },
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN 100 AND -100",
            vec![],
            IndexPruningMetrics {
                scanned_blocks: 0,
                pruned_blocks: 3,
            },
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN -9223372036854775808 AND -9223372036854775808",
            vec![0],
            IndexPruningMetrics {
                scanned_blocks: 1,
                pruned_blocks: 2,
            },
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN 9223372036854775807 AND 9223372036854775807",
            vec![11],
            IndexPruningMetrics {
                scanned_blocks: 1,
                pruned_blocks: 2,
            },
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN -9223372036854775808 AND 9223372036854775807",
            (0..=11).collect(),
            IndexPruningMetrics {
                scanned_blocks: 3,
                pruned_blocks: 0,
            },
        ),
        (
            "SELECT id FROM events WHERE key BETWEEN -8 AND 102 LIMIT 4 OFFSET 2",
            vec![4, 5, 6, 7],
            IndexPruningMetrics {
                scanned_blocks: 3,
                pruned_blocks: 0,
            },
        ),
    ] {
        assert_indexed_range(&mut indexed, &mut unindexed, sql, &expected, expected_delta);
    }
}

#[test]
fn equivalent_positive_range_conjunctions_use_the_documented_index_path() {
    let mut indexed = int64_database(true);
    let mut unindexed = int64_database(false);

    for sql in [
        "SELECT id FROM events WHERE key >= -8 AND key <= 1",
        "SELECT id FROM events WHERE key <= 1 AND key >= -8",
        "SELECT id FROM events WHERE -8 <= key AND 1 >= key",
        "SELECT id FROM events WHERE 1 >= key AND -8 <= key",
        "SELECT id FROM events WHERE NOT (key < -8 OR key > 1)",
    ] {
        assert_indexed_range(
            &mut indexed,
            &mut unindexed,
            sql,
            &[2, 3, 4, 5],
            IndexPruningMetrics {
                scanned_blocks: 2,
                pruned_blocks: 1,
            },
        );
    }

    for (sql, expected, expected_delta) in [
        (
            "SELECT id FROM events WHERE key <= 1 AND key >= 1",
            vec![5],
            IndexPruningMetrics {
                scanned_blocks: 1,
                pruned_blocks: 2,
            },
        ),
        (
            "SELECT id FROM events WHERE key <= -100 AND key >= 100",
            vec![],
            IndexPruningMetrics {
                scanned_blocks: 0,
                pruned_blocks: 3,
            },
        ),
        (
            "SELECT id FROM events WHERE 9223372036854775807 >= key AND \
             -9223372036854775808 <= key",
            (0..=11).collect(),
            IndexPruningMetrics {
                scanned_blocks: 3,
                pruned_blocks: 0,
            },
        ),
    ] {
        assert_indexed_range(&mut indexed, &mut unindexed, sql, &expected, expected_delta);
    }
}

#[test]
fn strict_range_conjunctions_prune_in_every_conjunction_and_operand_order() {
    let mut indexed = int64_database(true);
    let mut unindexed = int64_database(false);

    for (lower_forms, upper_forms) in [
        (["key > -8", "-8 < key"], ["key <= 1", "1 >= key"]),
        (["key >= -7", "-7 <= key"], ["key < 2", "2 > key"]),
        (["key > -8", "-8 < key"], ["key < 2", "2 > key"]),
    ] {
        for lower in lower_forms {
            for upper in upper_forms {
                for upper_first in [false, true] {
                    let conjunction = if upper_first {
                        format!("{upper} AND {lower}")
                    } else {
                        format!("{lower} AND {upper}")
                    };
                    let sql = format!("SELECT id FROM events WHERE {conjunction}");
                    assert_indexed_range(
                        &mut indexed,
                        &mut unindexed,
                        &sql,
                        &[3, 4, 5],
                        IndexPruningMetrics {
                            scanned_blocks: 2,
                            pruned_blocks: 1,
                        },
                    );
                }
            }
        }
    }
}

#[test]
fn strict_range_normalization_handles_equal_empty_reversed_and_extreme_bounds() {
    let mut indexed = int64_database(true);
    let mut unindexed = int64_database(false);

    for (sql, expected, expected_delta) in [
        (
            "SELECT id FROM events WHERE key > 0 AND key <= 1",
            vec![5],
            IndexPruningMetrics {
                scanned_blocks: 1,
                pruned_blocks: 2,
            },
        ),
        (
            "SELECT id FROM events WHERE key > 1 AND key <= 1",
            vec![],
            IndexPruningMetrics {
                scanned_blocks: 0,
                pruned_blocks: 3,
            },
        ),
        (
            "SELECT id FROM events WHERE key >= 100 AND key < -100",
            vec![],
            IndexPruningMetrics {
                scanned_blocks: 0,
                pruned_blocks: 3,
            },
        ),
        (
            "SELECT id FROM events WHERE key > 9223372036854775807 AND \
             key <= 9223372036854775807",
            vec![],
            IndexPruningMetrics {
                scanned_blocks: 0,
                pruned_blocks: 3,
            },
        ),
        (
            "SELECT id FROM events WHERE key >= -9223372036854775808 AND \
             key < -9223372036854775808",
            vec![],
            IndexPruningMetrics {
                scanned_blocks: 0,
                pruned_blocks: 3,
            },
        ),
        (
            "SELECT id FROM events WHERE key > -9223372036854775808 AND \
             key < 9223372036854775807",
            (1..=10).collect(),
            IndexPruningMetrics {
                scanned_blocks: 3,
                pruned_blocks: 0,
            },
        ),
    ] {
        assert_indexed_range(&mut indexed, &mut unindexed, sql, &expected, expected_delta);
    }
}

#[test]
fn nullable_ranges_prune_all_null_blocks_and_preserve_null_semantics() {
    const SETUP: &str = "\
        CREATE TABLE readings (value Nullable(Int64)); \
        INSERT INTO readings VALUES \
          (NULL), (NULL), (NULL), \
          (-5), (NULL), (0), \
          (5), (10), (NULL), \
          (9223372036854775807);";
    let mut indexed = Database::new();
    let mut unindexed = Database::new();
    indexed.execute(SETUP).expect("indexed setup");
    unindexed.execute(SETUP).expect("fallback setup");
    indexed
        .create_int64_min_max_index(
            "readings",
            "value",
            Int64MinMaxIndexLimits::new(3, 4, usize::MAX),
        )
        .expect("valid nullable index");

    assert_indexed_range(
        &mut indexed,
        &mut unindexed,
        "SELECT value FROM readings WHERE value < 6 AND value > -1",
        &[0, 5],
        IndexPruningMetrics {
            scanned_blocks: 2,
            pruned_blocks: 2,
        },
    );
    assert_indexed_range(
        &mut indexed,
        &mut unindexed,
        "SELECT value FROM readings WHERE 9223372036854775807 >= value AND \
         -9223372036854775808 <= value",
        &[-5, 0, 5, 10, i64::MAX],
        IndexPruningMetrics {
            scanned_blocks: 3,
            pruned_blocks: 1,
        },
    );

    let mut all_null = Database::new();
    all_null
        .execute(
            "CREATE TABLE missing (value Nullable(Int64)); \
             INSERT INTO missing VALUES (NULL), (NULL), (NULL), (NULL), (NULL), (NULL);",
        )
        .expect("all-null setup");
    all_null
        .create_int64_min_max_index(
            "missing",
            "value",
            Int64MinMaxIndexLimits::new(3, 2, usize::MAX),
        )
        .expect("valid all-null index");
    assert!(
        query(
            &mut all_null,
            "SELECT value FROM missing WHERE value <= 9223372036854775807 AND \
             value >= -9223372036854775808",
        )
        .rows
        .is_empty()
    );
    assert_eq!(
        all_null.index_pruning_metrics(),
        IndexPruningMetrics {
            scanned_blocks: 0,
            pruned_blocks: 2,
        }
    );
}

#[test]
fn unsupported_between_shapes_keep_the_exact_full_scan_fallback() {
    let mut indexed = int64_database(true);
    let mut unindexed = int64_database(false);

    for sql in [
        "SELECT id FROM events WHERE key NOT BETWEEN -8 AND 1",
        "SELECT id FROM events WHERE key BETWEEN -8.0 AND 1.0",
        "SELECT id FROM events WHERE key BETWEEN -8 AND 1 AND selected = true",
        "SELECT id FROM events WHERE id BETWEEN 2 AND 5",
    ] {
        assert_eq!(
            query(&mut indexed, sql),
            query(&mut unindexed, sql),
            "{sql}"
        );
        assert_eq!(
            indexed.index_pruning_metrics(),
            IndexPruningMetrics::default(),
            "unsupported shape must not record indexed work for {sql}",
        );
    }
}

#[test]
fn mutations_refresh_between_bounds_and_over_budget_growth_invalidates_them() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE changing (value Int64); \
             INSERT INTO changing VALUES (1), (2), (100), (101);",
        )
        .expect("setup");
    database
        .create_int64_min_max_index(
            "changing",
            "value",
            Int64MinMaxIndexLimits::new(2, 2, usize::MAX),
        )
        .expect("initial index fits");

    database
        .execute("ALTER TABLE changing UPDATE value = 50 WHERE value = 2")
        .expect("update rebuilds the index");
    assert_eq!(
        query(
            &mut database,
            "SELECT value FROM changing WHERE value <= 50 AND value >= 50",
        )
        .rows,
        rows(&[50]),
    );
    assert_eq!(
        database.index_pruning_metrics(),
        IndexPruningMetrics {
            scanned_blocks: 1,
            pruned_blocks: 1,
        },
        "the refreshed bounds admit the mutated row",
    );

    database
        .execute("INSERT INTO changing VALUES (75)")
        .expect("growth succeeds while invalidating over-budget metadata");
    assert!(
        database
            .catalog()
            .table("changing")
            .expect("table remains")
            .int64_min_max_index_info()
            .is_none()
    );
    let metrics = database.index_pruning_metrics();
    assert_eq!(
        query(
            &mut database,
            "SELECT value FROM changing WHERE 75 >= value AND 74 < value",
        )
        .rows,
        rows(&[75]),
    );
    assert_eq!(
        database.index_pruning_metrics(),
        metrics,
        "the invalidated index uses an unmetered full-scan fallback",
    );
}

#[test]
fn between_cannot_bypass_the_complete_source_scan_limit() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 11,
        ..QueryResultLimits::default()
    });
    database
        .execute(INT64_SETUP)
        .expect("setup is not a SELECT");
    database
        .create_int64_min_max_index(
            "events",
            "key",
            Int64MinMaxIndexLimits::new(4, 3, usize::MAX),
        )
        .expect("valid index request");

    assert_eq!(
        database.execute("SELECT id FROM events WHERE key < 100 AND key > 3 LIMIT 0"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT scanned rows",
            actual: 12,
            max: 11,
        })
    );
    assert_eq!(
        database.index_pruning_metrics(),
        IndexPruningMetrics::default(),
        "scan limits fail before all-prunable index work is recorded",
    );
}
