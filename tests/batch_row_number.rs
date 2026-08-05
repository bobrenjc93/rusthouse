use rusthouse::batch::engine::{Database, QueryResultLimits, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{self, BatchSqlLimits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::batch::{run_csv_batch, run_json_batch};

fn query(result: &StatementResult) -> &rusthouse::batch::engine::QueryResult {
    let StatementResult::Query(query) = result else {
        panic!("expected query result");
    };
    query
}

#[test]
fn row_number_is_a_mixed_aliased_projection_over_filtered_source_order() {
    let mut database = Database::new();
    let results = database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES \
                 (30, 'thirty'), (10, 'ten'), (20, 'twenty'), (40, 'forty'); \
             SELECT label AS event, ROW_NUMBER() OVER () AS sequence, id \
             FROM events WHERE id >= 20 LIMIT 2;",
        )
        .expect("ROW_NUMBER query succeeds");

    let result = query(&results[2]);
    assert_eq!(
        result.columns,
        vec![
            ResultColumn {
                name: "event".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "sequence".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("thirty".to_owned()),
                Value::Int64(1),
                Value::Int64(30),
            ],
            vec![
                Value::String("twenty".to_owned()),
                Value::Int64(2),
                Value::Int64(20),
            ],
        ]
    );
}

#[test]
fn row_number_handles_empty_filtered_and_limited_inputs() {
    let mut database = Database::new();
    let results = database
        .execute(
            "CREATE TABLE empty (id Int64); \
             CREATE TABLE events (id Int64); \
             INSERT INTO events VALUES (3), (1), (2); \
             SELECT ROW_NUMBER() OVER () FROM empty; \
             SELECT ROW_NUMBER() OVER () AS n FROM events WHERE id > 9; \
             SELECT id, ROW_NUMBER() OVER () AS n FROM events WHERE id >= 1 LIMIT 0; \
             SELECT id, ROW_NUMBER() OVER () AS n FROM events WHERE id >= 1 LIMIT 2;",
        )
        .expect("all supported boundary queries succeed");

    let empty = query(&results[3]);
    assert_eq!(
        empty.columns,
        vec![ResultColumn {
            name: "ROW_NUMBER()".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert!(empty.rows.is_empty());
    assert!(query(&results[4]).rows.is_empty());
    assert!(query(&results[5]).rows.is_empty());
    assert_eq!(
        query(&results[6]).rows,
        vec![
            vec![Value::Int64(3), Value::Int64(1)],
            vec![Value::Int64(1), Value::Int64(2)],
        ]
    );
}

#[test]
fn row_number_rejects_arguments_and_nonempty_window_specifications() {
    let malformed = [
        "SELECT ROW_NUMBER(id) OVER () FROM events",
        "SELECT ROW_NUMBER(*) OVER () FROM events",
        "SELECT ROW_NUMBER(1) OVER () FROM events",
        "SELECT ROW_NUMBER() FROM events",
        "SELECT ROW_NUMBER() OVER window_name FROM events",
        "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM events",
        "SELECT ROW_NUMBER() OVER (PARTITION BY id) FROM events",
        "SELECT ROW_NUMBER() OVER () FROM events WINDOW w AS ()",
    ];

    for sql in malformed {
        assert!(sql::parse(sql).is_err(), "must reject {sql:?}");
    }
}

#[test]
fn row_number_rejects_distinct_grouping_aggregates_and_ordering() {
    let unsupported = [
        "SELECT DISTINCT ROW_NUMBER() OVER () FROM events",
        "SELECT id, ROW_NUMBER() OVER () FROM events GROUP BY id",
        "SELECT ROW_NUMBER() OVER (), COUNT(*) FROM events",
        "SELECT ROW_NUMBER() OVER () FROM events HAVING n > 0",
        "SELECT id, ROW_NUMBER() OVER () FROM events ORDER BY id",
    ];

    for sql in unsupported {
        assert!(sql::parse(sql).is_err(), "must reject {sql:?}");
    }
}

#[test]
fn row_number_projection_obeys_ast_and_result_materialization_caps() {
    let ast_error = sql::parse_with_limits(
        "SELECT id, ROW_NUMBER() OVER () FROM events",
        BatchSqlLimits {
            max_ast_list_items: 1,
            ..BatchSqlLimits::default()
        },
    )
    .expect_err("the second projection crosses the AST list cap");
    assert_eq!(
        ast_error,
        Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        }
    );

    let mut row_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        ..QueryResultLimits::default()
    });
    let row_error = row_limited
        .execute(
            "CREATE TABLE events (id Int64); \
             INSERT INTO events VALUES (1), (2); \
             SELECT ROW_NUMBER() OVER () FROM events;",
        )
        .expect_err("two window rows cross the result row cap");
    assert_eq!(
        row_error,
        Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 2,
            max: 1,
        }
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_values: 3,
        ..QueryResultLimits::default()
    });
    let value_error = value_limited
        .execute(
            "CREATE TABLE events (id Int64); \
             INSERT INTO events VALUES (1), (2); \
             SELECT id, ROW_NUMBER() OVER () FROM events;",
        )
        .expect_err("the window projection counts toward result values");
    assert_eq!(
        value_error,
        Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 4,
            max: 3,
        }
    );
}

#[test]
fn row_number_is_rendered_as_typed_csv_and_json() {
    let input = b"CREATE TABLE events (id Int64); \
        INSERT INTO events VALUES (7), (9); \
        SELECT id, ROW_NUMBER() OVER () AS sequence FROM events;";

    let mut csv = Vec::new();
    run_csv_batch(&input[..], &mut csv).expect("CSV output succeeds");
    assert_eq!(csv, b"id,sequence\n7,1\n9,2\n");

    let mut json = Vec::new();
    run_json_batch(&input[..], &mut json).expect("JSON output succeeds");
    assert_eq!(
        json,
        concat!(
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"sequence","type":"Int64"}],"rows":[[7,1],[9,2]]}"#,
            "\n"
        )
        .as_bytes()
    );
}
