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

#[test]
fn parses_only_the_bounded_wildcard_cross_join_shape() {
    let statements = parse("select * from LeftRows cross join RightRows limit 3;")
        .expect("the exact CROSS JOIN shape is valid");

    let [Statement::CrossJoin(join)] = statements.as_slice() else {
        panic!("expected one CROSS JOIN statement");
    };
    assert_eq!(join.left_table, "LeftRows");
    assert_eq!(join.right_table, "RightRows");
    assert_eq!(join.limit, Some(3));

    for sql in [
        "SELECT id FROM left_rows CROSS JOIN right_rows",
        "SELECT *, id FROM left_rows CROSS JOIN right_rows",
        "SELECT * AS everything FROM left_rows CROSS JOIN right_rows",
        "SELECT * FROM left_rows AS l CROSS JOIN right_rows",
        "SELECT * FROM left_rows l CROSS JOIN right_rows",
        "SELECT * FROM left_rows CROSS JOIN right_rows AS r",
        "SELECT * FROM left_rows CROSS JOIN right_rows r",
        "SELECT * FROM left_rows WHERE id = 1 CROSS JOIN right_rows",
        "SELECT * FROM left_rows CROSS JOIN right_rows WHERE id = 1",
        "SELECT * FROM left_rows CROSS JOIN right_rows CROSS JOIN third_rows",
        "SELECT * FROM left_rows CROSS JOIN right_rows INNER JOIN third_rows ON id = id",
        "SELECT * FROM left_rows CROSS JOIN right_rows UNION ALL SELECT * FROM third_rows",
        "SELECT * FROM left_rows LIMIT 1 CROSS JOIN right_rows",
        "SELECT * FROM left_rows CROSS JOIN right_rows LIMIT 1 LIMIT 1",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn returns_all_four_types_and_columns_in_left_major_order() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE left_rows (id Int64, label String); \
             CREATE TABLE right_rows (score Float64, active Bool); \
             INSERT INTO left_rows VALUES (1, 'first'), (2, 'second'); \
             INSERT INTO right_rows VALUES (1.5, true), (2.5, false);",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT * FROM left_rows CROSS JOIN right_rows",
    );

    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "score".to_owned(),
                data_type: DataType::Float64,
            },
            ResultColumn {
                name: "active".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![
                Value::Int64(1),
                Value::String("first".to_owned()),
                Value::Float64(1.5),
                Value::Bool(true),
            ],
            vec![
                Value::Int64(1),
                Value::String("first".to_owned()),
                Value::Float64(2.5),
                Value::Bool(false),
            ],
            vec![
                Value::Int64(2),
                Value::String("second".to_owned()),
                Value::Float64(1.5),
                Value::Bool(true),
            ],
            vec![
                Value::Int64(2),
                Value::String("second".to_owned()),
                Value::Float64(2.5),
                Value::Bool(false),
            ],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT * FROM left_rows CROSS JOIN right_rows LIMIT 3",
        )
        .rows,
        result.rows[..3]
    );
}

#[test]
fn either_empty_input_returns_the_combined_typed_schema_and_no_rows() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE empty_left (id Int64); \
             CREATE TABLE populated_left (id Int64); \
             CREATE TABLE empty_right (label String); \
             CREATE TABLE populated_right (label String); \
             INSERT INTO populated_left VALUES (1); \
             INSERT INTO populated_right VALUES ('present');",
        )
        .expect("setup succeeds");

    for sql in [
        "SELECT * FROM empty_left CROSS JOIN populated_right",
        "SELECT * FROM populated_left CROSS JOIN empty_right",
        "SELECT * FROM empty_left CROSS JOIN empty_right",
    ] {
        let result = query(&mut database, sql);
        assert_eq!(
            result.columns,
            [
                ResultColumn {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ]
        );
        assert!(result.rows.is_empty());
    }
}

fn database_with_limits(limits: QueryResultLimits) -> Database {
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE left_rows (id Int64, label String); \
             CREATE TABLE right_rows (score Float64, active Bool, note String); \
             INSERT INTO left_rows VALUES (1, 'a'), (2, 'bb'); \
             INSERT INTO right_rows VALUES (1.5, true, 'x'), (2.5, false, 'yyy');",
        )
        .expect("setup succeeds");
    database
}

#[test]
fn validates_limit_reduced_row_value_and_byte_counts_at_exact_boundaries() {
    const ROWS: usize = 3;
    const COLUMNS: usize = 5;
    const STRING_BYTES: usize = 9;
    let column_name_bytes =
        "id".len() + "label".len() + "score".len() + "active".len() + "note".len();
    let exact_bytes = COLUMNS * std::mem::size_of::<ResultColumn>()
        + column_name_bytes
        + ROWS * std::mem::size_of::<Vec<Value>>()
        + ROWS * COLUMNS * std::mem::size_of::<Value>()
        + STRING_BYTES;
    let exact_limits = QueryResultLimits {
        max_rows: ROWS,
        max_values: ROWS * COLUMNS,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let sql = "SELECT * FROM left_rows CROSS JOIN right_rows LIMIT 3";

    assert_eq!(
        query(&mut database_with_limits(exact_limits), sql)
            .rows
            .len(),
        ROWS
    );

    for (limits, expected) in [
        (
            QueryResultLimits {
                max_rows: ROWS - 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result rows",
                actual: ROWS,
                max: ROWS - 1,
            },
        ),
        (
            QueryResultLimits {
                max_values: ROWS * COLUMNS - 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result values",
                actual: ROWS * COLUMNS,
                max: ROWS * COLUMNS - 1,
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
                .expect_err("the limited result exceeds its configured cap"),
            expected
        );
    }
}

#[test]
fn zero_limit_materializes_only_the_schema_and_shared_queries_accept_cross_join() {
    let schema_bytes = 2 * std::mem::size_of::<ResultColumn>() + "id".len() + "flag".len();
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 0,
        max_values: 0,
        max_bytes: schema_bytes,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE left_rows (id Int64); \
             CREATE TABLE right_rows (flag Bool); \
             INSERT INTO left_rows VALUES (1); \
             INSERT INTO right_rows VALUES (true);",
        )
        .expect("setup succeeds");
    let result = query(
        &mut database,
        "SELECT * FROM left_rows CROSS JOIN right_rows LIMIT 0",
    );
    assert!(result.rows.is_empty());

    let shared = SharedDatabase::new(database);
    let result = shared
        .query("SELECT * FROM left_rows CROSS JOIN right_rows LIMIT 0")
        .expect("CROSS JOIN is a read-only shared query");
    assert!(result.rows.is_empty());
}
