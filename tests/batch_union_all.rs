use rusthouse::SharedDatabase;
use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn database_with_limits(limits: QueryResultLimits) -> Database {
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE left_rows (n Int64); \
             CREATE TABLE right_rows (n Int64); \
             INSERT INTO left_rows VALUES (1), (2); \
             INSERT INTO right_rows VALUES (3), (4);",
        )
        .expect("setup succeeds");
    database
}

#[test]
fn parses_exactly_two_complete_select_operands() {
    let statements = parse(
        "SELECT DISTINCT kind FROM left_events LIMIT 2 \
         UNION ALL \
         SELECT kind FROM right_events WHERE active = true ORDER BY kind LIMIT 3;",
    )
    .expect("two SELECT operands are valid");

    let [Statement::UnionAll { left, right }] = statements.as_slice() else {
        panic!("expected one UNION ALL statement");
    };
    assert!(left.distinct);
    assert_eq!(left.table, "left_events");
    assert_eq!(left.limit, Some(2));
    assert_eq!(right.table, "right_events");
    assert!(right.predicate.is_some());
    assert_eq!(right.order_by.len(), 1);
    assert_eq!(right.limit, Some(3));
}

#[test]
fn rejects_malformed_nested_and_outer_union_syntax() {
    for sql in [
        "SELECT n FROM l UNION SELECT n FROM r",
        "SELECT n FROM l UNION DISTINCT SELECT n FROM r",
        "SELECT n FROM l UNION ALL",
        "SELECT n FROM l UNION ALL SHOW TABLES",
        "SELECT n FROM l UNION ALL (SELECT n FROM r)",
        "SELECT n FROM l UNION ALL SELECT n FROM r UNION ALL SELECT n FROM x",
        "SELECT n FROM l UNION ALL SELECT n FROM r FORMAT CSV",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn concatenates_filtered_projections_left_first_with_left_column_names() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE older (id Int64, label String); \
             CREATE TABLE newer (id Int64, label String); \
             INSERT INTO older VALUES (1, 'skip'), (2, 'old-2'), (3, 'old-3'); \
             INSERT INTO newer VALUES (4, 'new-4'), (5, 'skip'), (6, 'new-6');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT id AS event_id, label AS description FROM older WHERE id >= 2 \
         UNION ALL \
         SELECT id AS ignored_id, label AS ignored_label FROM newer WHERE id != 5",
    );

    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "event_id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "description".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![Value::Int64(2), Value::String("old-2".to_owned())],
            vec![Value::Int64(3), Value::String("old-3".to_owned())],
            vec![Value::Int64(4), Value::String("new-4".to_owned())],
            vec![Value::Int64(6), Value::String("new-6".to_owned())],
        ]
    );
}

#[test]
fn supports_either_or_both_empty_operands() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE empty_left (n Int64); \
             CREATE TABLE populated (n Int64); \
             CREATE TABLE empty_right (n Int64); \
             INSERT INTO populated VALUES (7), (8);",
        )
        .expect("setup succeeds");

    assert_eq!(
        query(
            &mut database,
            "SELECT n AS from_left FROM empty_left \
             UNION ALL SELECT n AS from_right FROM populated",
        )
        .rows,
        [vec![Value::Int64(7)], vec![Value::Int64(8)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT n FROM populated UNION ALL SELECT n FROM empty_right",
        )
        .rows,
        [vec![Value::Int64(7)], vec![Value::Int64(8)]]
    );
    assert!(
        query(
            &mut database,
            "SELECT n FROM empty_left UNION ALL SELECT n FROM empty_right",
        )
        .rows
        .is_empty()
    );
}

#[test]
fn rejects_column_count_and_type_mismatches() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE left_rows (id Int64, label String); \
             CREATE TABLE right_rows (id Int64, label String);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database
            .execute("SELECT id FROM left_rows UNION ALL SELECT id, label FROM right_rows")
            .expect_err("column counts must match"),
        Error::UnionColumnCountMismatch { left: 1, right: 2 }
    );
    assert_eq!(
        database
            .execute("SELECT id FROM left_rows UNION ALL SELECT label FROM right_rows")
            .expect_err("column types must match"),
        Error::TypeMismatch {
            context: "UNION ALL column 1".to_owned(),
            expected: "Int64".to_owned(),
            actual: "String".to_owned(),
        }
    );
}

#[test]
fn enforces_combined_row_value_and_byte_limits_at_the_boundary() {
    let exact_bytes = std::mem::size_of::<ResultColumn>()
        + "n".len()
        + 4 * std::mem::size_of::<Vec<Value>>()
        + 4 * std::mem::size_of::<Value>();
    let exact_limits = QueryResultLimits {
        max_rows: 4,
        max_values: 4,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let sql = "SELECT n FROM left_rows UNION ALL SELECT n FROM right_rows";
    assert_eq!(
        query(&mut database_with_limits(exact_limits), sql)
            .rows
            .len(),
        4
    );

    for (limits, expected) in [
        (
            QueryResultLimits {
                max_rows: 3,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result rows",
                actual: 4,
                max: 3,
            },
        ),
        (
            QueryResultLimits {
                max_values: 3,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result values",
                actual: 4,
                max: 3,
            },
        ),
        (
            QueryResultLimits {
                max_bytes: exact_bytes - 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            },
        ),
    ] {
        assert_eq!(
            database_with_limits(limits)
                .execute(sql)
                .expect_err("combined result exceeds its cap"),
            expected
        );
    }
}

#[test]
fn shared_database_accepts_union_all_as_one_read_only_query() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE first (n Int64); CREATE TABLE second (n Int64); \
             INSERT INTO first VALUES (1); INSERT INTO second VALUES (2);",
        )
        .expect("setup succeeds");

    let result = database
        .query("SELECT n FROM first UNION ALL SELECT n FROM second")
        .expect("UNION ALL is read-only");
    assert_eq!(result.rows, [vec![Value::Int64(1)], vec![Value::Int64(2)]]);
}
