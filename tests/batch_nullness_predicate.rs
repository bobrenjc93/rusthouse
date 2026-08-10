use rusthouse::batch::engine::{Database, QueryResult, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Predicate, Statement, parse};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{
    IndexPruningMetrics, Int64MinMaxBlockMetadata, Int64MinMaxIndexAdmission,
    Int64MinMaxIndexLimits,
};

fn predicate(sql: &str) -> Predicate {
    let statements = parse(sql).expect("valid predicate query");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    select.predicate.clone().expect("WHERE predicate")
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    result.clone()
}

fn assert_indexed_differential(
    indexed: &mut Database,
    unindexed: &mut Database,
    sql: &str,
    expected_delta: IndexPruningMetrics,
) {
    let before = indexed.index_pruning_metrics();
    assert_eq!(query(indexed, sql), query(unindexed, sql), "{sql}");
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
        "unindexed query must retain exact fallback for {sql}",
    );
}

#[test]
fn parses_case_insensitive_nullness_atoms_with_boolean_precedence() {
    assert_eq!(
        predicate("SELECT value FROM readings WHERE value iS nUlL"),
        Predicate::IsNull {
            column: "value".to_owned(),
        }
    );
    assert_eq!(
        predicate("SELECT DISTINCT value FROM readings WHERE value Is NoT NuLl"),
        Predicate::IsNotNull {
            column: "value".to_owned(),
        }
    );
    assert_eq!(
        predicate(
            "SELECT value FROM readings \
             WHERE NOT value IS NULL AND value IS NOT NULL OR (value IS NULL)",
        ),
        Predicate::Or(
            Box::new(Predicate::And(
                Box::new(Predicate::Not(Box::new(Predicate::IsNull {
                    column: "value".to_owned(),
                }))),
                Box::new(Predicate::IsNotNull {
                    column: "value".to_owned(),
                }),
            )),
            Box::new(Predicate::IsNull {
                column: "value".to_owned(),
            }),
        )
    );

    assert_eq!(
        predicate("SELECT not FROM keywords WHERE not IS NULL"),
        Predicate::IsNull {
            column: "not".to_owned(),
        },
        "a column named 'not' remains an operand before IS NULL",
    );
}

#[test]
fn rejects_malformed_nullness_syntax_in_regular_and_distinct_selects() {
    for malformed in [
        "value IS",
        "value IS NOT",
        "value IS TRUE",
        "value IS NOT NOT NULL",
        "value NOT IS NULL",
        "value IS NULL NULL",
        "1 IS NULL",
        "(value IS NULL",
    ] {
        for projection in ["value", "DISTINCT value"] {
            let sql = format!("SELECT {projection} FROM readings WHERE {malformed}");
            assert!(
                parse(&sql).is_err(),
                "malformed predicate was accepted: {sql}"
            );
        }
    }
}

#[test]
fn sql_created_nullable_table_composes_nullness_with_grouping_order_and_pagination() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (measurement Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL), (3), (NULL), (1), (2), (NULL), (3);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT measurement FROM readings \
             WHERE measurement IS NULL OR \
                   (measurement IS NOT NULL AND NOT measurement = 2) \
             ORDER BY measurement DESC LIMIT 4 OFFSET 1",
        )
        .rows,
        [
            vec![Value::Int64(3)],
            vec![Value::Int64(1)],
            vec![Value::Null(DataType::Int64)],
            vec![Value::Null(DataType::Int64)],
        ]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT COUNT(*) AS rows, COUNT(measurement) AS present, \
                    SUM(measurement) AS total \
             FROM readings WHERE measurement IS NOT NULL",
        )
        .rows,
        [vec![Value::Int64(4), Value::Int64(4), Value::Int64(9)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT measurement, COUNT(*) AS n FROM readings \
             WHERE measurement IS NULL OR measurement = 3 \
             GROUP BY measurement ORDER BY measurement",
        )
        .rows,
        [
            vec![Value::Null(DataType::Int64), Value::Int64(3),],
            vec![Value::Int64(3), Value::Int64(2)],
        ]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT measurement FROM readings \
             WHERE measurement IS NOT NULL ORDER BY measurement DESC LIMIT 2",
        )
        .rows,
        [vec![Value::Int64(3)], vec![Value::Int64(2)]]
    );
}

#[test]
fn non_nullable_columns_have_constant_sql_nullness_semantics() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one'), (2, 2.5, false, 'two');",
        )
        .expect("setup");

    assert!(
        query(
            &mut database,
            "SELECT i FROM samples \
             WHERE i IS NULL OR f IS NULL OR b IS NULL OR s IS NULL",
        )
        .rows
        .is_empty()
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT COUNT(*) AS n FROM samples \
             WHERE i IS NOT NULL AND f IS NOT NULL AND b IS NOT NULL AND s IS NOT NULL",
        )
        .rows,
        [vec![Value::Int64(2)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT i FROM samples WHERE NOT (i IS NULL OR s IS NULL) ORDER BY i",
        )
        .rows,
        [vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
}

fn nullness_test_database() -> Database {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (value Nullable(Int64)); \
             INSERT INTO readings VALUES \
               (NULL), (NULL), (NULL), \
               (1), (2), (3), \
               (NULL), (4), (NULL), \
               (5);",
        )
        .expect("setup");
    database
}

#[test]
fn indexed_nullness_matches_fallback_across_block_shapes_and_rebuilds() {
    let mut indexed = nullness_test_database();
    let mut unindexed = nullness_test_database();
    assert!(matches!(
        indexed
            .create_int64_min_max_index(
                "readings",
                "value",
                Int64MinMaxIndexLimits::new(3, 4, usize::MAX),
            )
            .expect("valid index request"),
        Int64MinMaxIndexAdmission::Created(_)
    ));
    assert_eq!(
        indexed
            .catalog()
            .table("readings")
            .unwrap()
            .int64_min_max_index_blocks()
            .unwrap(),
        [
            Int64MinMaxBlockMetadata {
                first_row: 0,
                row_count: 3,
                min: None,
                max: None,
                null_count: 3,
            },
            Int64MinMaxBlockMetadata {
                first_row: 3,
                row_count: 3,
                min: Some(1),
                max: Some(3),
                null_count: 0,
            },
            Int64MinMaxBlockMetadata {
                first_row: 6,
                row_count: 3,
                min: Some(4),
                max: Some(4),
                null_count: 2,
            },
            Int64MinMaxBlockMetadata {
                first_row: 9,
                row_count: 1,
                min: Some(5),
                max: Some(5),
                null_count: 0,
            },
        ]
    );

    for (sql, expected_delta) in [
        (
            "SELECT value FROM readings WHERE value IS NULL ORDER BY value",
            IndexPruningMetrics {
                scanned_blocks: 2,
                pruned_blocks: 2,
            },
        ),
        (
            "SELECT value FROM readings WHERE value IS NOT NULL ORDER BY value",
            IndexPruningMetrics {
                scanned_blocks: 3,
                pruned_blocks: 1,
            },
        ),
        (
            "SELECT value FROM readings WHERE NOT value IS NULL ORDER BY value",
            IndexPruningMetrics {
                scanned_blocks: 3,
                pruned_blocks: 1,
            },
        ),
        (
            "SELECT value FROM readings WHERE NOT (value IS NOT NULL) ORDER BY value",
            IndexPruningMetrics {
                scanned_blocks: 2,
                pruned_blocks: 2,
            },
        ),
    ] {
        assert_indexed_differential(&mut indexed, &mut unindexed, sql, expected_delta);
    }

    let mutation = "ALTER TABLE readings UPDATE value = NULL WHERE value = 4; \
                    INSERT INTO readings VALUES (NULL), (6);";
    indexed.execute(mutation).expect("indexed mutation");
    unindexed.execute(mutation).expect("unindexed mutation");
    assert_eq!(
        indexed
            .catalog()
            .table("readings")
            .unwrap()
            .int64_min_max_index_blocks()
            .unwrap(),
        [
            Int64MinMaxBlockMetadata {
                first_row: 0,
                row_count: 3,
                min: None,
                max: None,
                null_count: 3,
            },
            Int64MinMaxBlockMetadata {
                first_row: 3,
                row_count: 3,
                min: Some(1),
                max: Some(3),
                null_count: 0,
            },
            Int64MinMaxBlockMetadata {
                first_row: 6,
                row_count: 3,
                min: None,
                max: None,
                null_count: 3,
            },
            Int64MinMaxBlockMetadata {
                first_row: 9,
                row_count: 3,
                min: Some(5),
                max: Some(6),
                null_count: 1,
            },
        ],
        "successful mutations rebuild null counts before the next query",
    );

    assert_indexed_differential(
        &mut indexed,
        &mut unindexed,
        "SELECT value FROM readings WHERE value IS NULL ORDER BY value",
        IndexPruningMetrics {
            scanned_blocks: 3,
            pruned_blocks: 1,
        },
    );
    assert_indexed_differential(
        &mut indexed,
        &mut unindexed,
        "SELECT value FROM readings WHERE value IS NOT NULL ORDER BY value",
        IndexPruningMetrics {
            scanned_blocks: 2,
            pruned_blocks: 2,
        },
    );
    assert_indexed_differential(
        &mut indexed,
        &mut unindexed,
        "SELECT value FROM readings WHERE NOT value IS NULL ORDER BY value",
        IndexPruningMetrics {
            scanned_blocks: 2,
            pruned_blocks: 2,
        },
    );
    assert_indexed_differential(
        &mut indexed,
        &mut unindexed,
        "SELECT value FROM readings WHERE NOT (value IS NOT NULL) ORDER BY value",
        IndexPruningMetrics {
            scanned_blocks: 3,
            pruned_blocks: 1,
        },
    );
}

#[test]
fn compound_nullness_predicates_use_exact_unindexed_fallback() {
    let mut indexed = nullness_test_database();
    let mut unindexed = nullness_test_database();
    indexed
        .create_int64_min_max_index(
            "readings",
            "value",
            Int64MinMaxIndexLimits::new(3, 4, usize::MAX),
        )
        .expect("valid index request");

    for sql in [
        "SELECT value FROM readings WHERE value IS NULL OR value = 2 ORDER BY value",
        "SELECT value FROM readings WHERE value IS NOT NULL AND value >= 3 ORDER BY value",
        "SELECT value FROM readings WHERE NOT (value IS NULL OR value = 2) ORDER BY value",
    ] {
        assert_eq!(
            query(&mut indexed, sql),
            query(&mut unindexed, sql),
            "{sql}"
        );
    }
    assert_eq!(
        indexed.index_pruning_metrics(),
        IndexPruningMetrics::default(),
    );
}

#[test]
fn nullness_cannot_bypass_the_source_scan_limit_with_limit_zero() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE readings (value Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL), (1), (NULL);",
        )
        .expect("setup");
    database
        .create_int64_min_max_index(
            "readings",
            "value",
            Int64MinMaxIndexLimits::new(1, 3, usize::MAX),
        )
        .expect("valid index request");

    assert_eq!(
        database.execute("SELECT value FROM readings WHERE value IS NULL LIMIT 0"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(
        database.index_pruning_metrics(),
        IndexPruningMetrics::default(),
        "the full source charge fails before indexed work is recorded",
    );
}
