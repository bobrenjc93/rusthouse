use rusthouse::batch::engine::{Database, QueryResult, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn create_mixed(database: &mut Database) {
    database
        .create_nullable_int64_table(
            "mixed",
            "v",
            vec![None, Some(2), Some(2), None, Some(5), Some(-1), Some(5)],
        )
        .expect("nullable table setup");
}

#[test]
fn global_count_distinguishes_empty_all_null_and_mixed_inputs() {
    let mut database = Database::new();
    database
        .create_nullable_int64_table("empty", "v", vec![])
        .unwrap();
    database
        .create_nullable_int64_table("all_null", "v", vec![None, None, None])
        .unwrap();
    create_mixed(&mut database);

    for (table, expected) in [
        ("empty", vec![Value::Int64(0), Value::Int64(0)]),
        ("all_null", vec![Value::Int64(0), Value::Int64(3)]),
        ("mixed", vec![Value::Int64(5), Value::Int64(7)]),
    ] {
        let result = query(
            &mut database,
            &format!("SELECT COUNT(v), COUNT(*) FROM {table}"),
        );
        assert_eq!(result.rows, [expected], "table {table}");
    }

    let empty_groups = query(&mut database, "SELECT v, COUNT(v) FROM empty GROUP BY v");
    assert!(empty_groups.rows.is_empty());
}

#[test]
fn nullable_count_composes_with_filters_having_ordering_and_pagination() {
    let mut database = Database::new();
    create_mixed(&mut database);

    let global = query(
        &mut database,
        "SELECT COUNT(v) AS present, COUNT(*) AS rows FROM mixed \
         WHERE v >= 2 HAVING present = 4 \
         ORDER BY present DESC LIMIT 1 OFFSET 0",
    );
    assert_eq!(global.rows, [vec![Value::Int64(4), Value::Int64(4)]]);

    let grouped = query(
        &mut database,
        "SELECT v, COUNT(v) AS present, COUNT(*) AS rows FROM mixed \
         GROUP BY v HAVING rows >= 2 \
         ORDER BY present DESC, v ASC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        grouped.rows,
        [
            vec![Value::Int64(5), Value::Int64(2), Value::Int64(2)],
            vec![
                Value::Null(DataType::Int64),
                Value::Int64(0),
                Value::Int64(2),
            ],
        ]
    );
}

#[test]
fn nullable_count_preserves_query_resource_bounds() {
    let exact_limits = QueryResultLimits {
        max_scan_rows: 7,
        max_rows: 2,
        max_values: 6,
        max_groups: 4,
        max_aggregate_state_cells: 12,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
    create_mixed(&mut exact);
    assert_eq!(
        query(
            &mut exact,
            "SELECT v, COUNT(v), COUNT(*) FROM mixed GROUP BY v \
             ORDER BY v ASC LIMIT 2",
        )
        .rows
        .len(),
        2
    );

    for (limits, expected) in [
        (
            QueryResultLimits {
                max_scan_rows: 6,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT scanned rows",
                actual: 7,
                max: 6,
            },
        ),
        (
            QueryResultLimits {
                max_groups: 3,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT groups",
                actual: 4,
                max: 3,
            },
        ),
        (
            QueryResultLimits {
                max_aggregate_state_cells: 6,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state cells",
                actual: 7,
                max: 6,
            },
        ),
    ] {
        let mut database = Database::with_query_result_limits(limits);
        create_mixed(&mut database);
        assert_eq!(
            database
                .execute("SELECT v, COUNT(v) FROM mixed GROUP BY v")
                .unwrap_err(),
            expected
        );
    }
}

#[test]
fn every_nullable_int64_aggregate_coexists() {
    let mut database = Database::new();
    create_mixed(&mut database);

    assert_eq!(
        query(
            &mut database,
            "SELECT COUNT(v), SUM(v), MIN(v), MAX(v), AVG(v) FROM mixed"
        )
        .rows,
        [vec![
            Value::Int64(5),
            Value::Int64(13),
            Value::Int64(-1),
            Value::Int64(5),
            Value::Float64(2.6),
        ]]
    );
}
