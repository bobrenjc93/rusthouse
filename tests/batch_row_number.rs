use rusthouse::batch::engine::{
    Database, ESTIMATED_GROUP_KEY_CELL_BYTES, QueryResultLimits, ResultColumn, StatementResult,
};
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
fn partitioned_row_number_filters_then_counts_interleaved_partitions_before_limit() {
    let mut database = Database::new();
    let results = database
        .execute(
            "CREATE TABLE events (id Int64, partition_key Int64, label String); \
             INSERT INTO events VALUES \
                 (1, 10, 'filtered'), (2, 20, 'twenty-first'), \
                 (3, 10, 'ten-first'), (4, 20, 'twenty-second'), \
                 (5, 30, 'thirty-first'), (6, 10, 'ten-second'); \
             SELECT id, partition_key, label, \
                    ROW_NUMBER() OVER (PARTITION BY partition_key) AS n \
             FROM events WHERE id >= 2 LIMIT 4;",
        )
        .expect("partitioned ROW_NUMBER query succeeds");

    assert_eq!(
        query(&results[2]).rows,
        vec![
            vec![
                Value::Int64(2),
                Value::Int64(20),
                Value::String("twenty-first".to_owned()),
                Value::Int64(1),
            ],
            vec![
                Value::Int64(3),
                Value::Int64(10),
                Value::String("ten-first".to_owned()),
                Value::Int64(1),
            ],
            vec![
                Value::Int64(4),
                Value::Int64(20),
                Value::String("twenty-second".to_owned()),
                Value::Int64(2),
            ],
            vec![
                Value::Int64(5),
                Value::Int64(30),
                Value::String("thirty-first".to_owned()),
                Value::Int64(1),
            ],
        ]
    );
}

#[test]
fn ordered_row_number_supports_both_directions_stable_ties_filtering_and_limit() {
    let mut database = Database::new();
    let results = database
        .execute(
            "CREATE TABLE events (id Int64, rank_key Int64, label String); \
             INSERT INTO events VALUES \
                 (1, 2, 'first-two'), (2, 1, 'first-one'), \
                 (3, 2, 'second-two'), (4, 1, 'second-one'), \
                 (5, 3, 'three'), (6, 0, 'filtered'); \
             SELECT id, label, ROW_NUMBER() OVER (ORDER BY rank_key ASC) AS n \
             FROM events WHERE id <= 5 LIMIT 4; \
             SELECT id, label, ROW_NUMBER() OVER (ORDER BY rank_key DESC) AS n \
             FROM events WHERE id <= 5 LIMIT 4;",
        )
        .expect("ordered ROW_NUMBER queries succeed");

    assert_eq!(
        query(&results[2]).rows,
        vec![
            vec![
                Value::Int64(2),
                Value::String("first-one".to_owned()),
                Value::Int64(1),
            ],
            vec![
                Value::Int64(4),
                Value::String("second-one".to_owned()),
                Value::Int64(2),
            ],
            vec![
                Value::Int64(1),
                Value::String("first-two".to_owned()),
                Value::Int64(3),
            ],
            vec![
                Value::Int64(3),
                Value::String("second-two".to_owned()),
                Value::Int64(4),
            ],
        ]
    );
    assert_eq!(
        query(&results[3]).rows,
        vec![
            vec![
                Value::Int64(5),
                Value::String("three".to_owned()),
                Value::Int64(1),
            ],
            vec![
                Value::Int64(1),
                Value::String("first-two".to_owned()),
                Value::Int64(2),
            ],
            vec![
                Value::Int64(3),
                Value::String("second-two".to_owned()),
                Value::Int64(3),
            ],
            vec![
                Value::Int64(2),
                Value::String("first-one".to_owned()),
                Value::Int64(4),
            ],
        ]
    );
}

#[test]
fn ordered_row_number_limits_rows_before_checked_cast_projection() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (rank_key Int64, reading Float64); \
             INSERT INTO samples VALUES (2, 9223372036854775808.0), (1, 7.9);",
        )
        .expect("fixture succeeds");

    let results = database
        .execute(
            "SELECT CAST(reading AS Int64) AS converted, \
                    ROW_NUMBER() OVER (ORDER BY rank_key ASC) AS n \
             FROM samples LIMIT 1",
        )
        .expect("window LIMIT excludes the overflowing cast");
    assert_eq!(
        query(&results[0]).rows,
        [vec![Value::Int64(7), Value::Int64(1)]]
    );

    assert_eq!(
        database.execute(
            "SELECT CAST(reading AS Int64), \
                    ROW_NUMBER() OVER (ORDER BY rank_key DESC) \
             FROM samples LIMIT 1",
        ),
        Err(Error::NumericOverflow("CAST(Float64 AS Int64)".to_owned()))
    );
}

#[test]
fn ordered_row_number_rejects_missing_and_non_int64_columns() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (1, 'one');",
        )
        .expect("fixture succeeds");

    assert_eq!(
        database
            .execute("SELECT ROW_NUMBER() OVER (ORDER BY missing ASC) FROM events")
            .expect_err("missing ordering column is rejected"),
        Error::ColumnNotFound {
            table: "events".to_owned(),
            column: "missing".to_owned(),
        }
    );
    assert_eq!(
        database
            .execute("SELECT ROW_NUMBER() OVER (ORDER BY label DESC) FROM events")
            .expect_err("non-Int64 ordering column is rejected"),
        Error::TypeMismatch {
            context: "ROW_NUMBER ORDER BY column 'label'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "String".to_owned(),
        }
    );
}

#[test]
fn partitioned_row_number_rejects_missing_and_non_int64_columns() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (1, 'one');",
        )
        .expect("fixture succeeds");

    assert_eq!(
        database
            .execute("SELECT ROW_NUMBER() OVER (PARTITION BY missing) FROM events")
            .expect_err("missing partition column is rejected"),
        Error::ColumnNotFound {
            table: "events".to_owned(),
            column: "missing".to_owned(),
        }
    );
    assert_eq!(
        database
            .execute("SELECT ROW_NUMBER() OVER (PARTITION BY label) FROM events")
            .expect_err("non-Int64 partition column is rejected"),
        Error::TypeMismatch {
            context: "ROW_NUMBER PARTITION BY column 'label'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "String".to_owned(),
        }
    );
}

#[test]
fn row_number_rejects_arguments_and_unsupported_window_specifications() {
    let malformed = [
        "SELECT ROW_NUMBER(id) OVER () FROM events",
        "SELECT ROW_NUMBER(*) OVER () FROM events",
        "SELECT ROW_NUMBER(1) OVER () FROM events",
        "SELECT ROW_NUMBER() FROM events",
        "SELECT ROW_NUMBER() OVER window_name FROM events",
        "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM events",
        "SELECT ROW_NUMBER() OVER (ORDER BY id ASC, id DESC) FROM events",
        "SELECT ROW_NUMBER() OVER (PARTITION BY id, other) FROM events",
        "SELECT ROW_NUMBER() OVER (PARTITION BY id ORDER BY other ASC) FROM events",
        "SELECT ROW_NUMBER() OVER (PARTITION BY id ASC) FROM events",
        "SELECT ROW_NUMBER() OVER () FROM events WINDOW w AS ()",
    ];

    for sql in malformed {
        assert!(sql::parse(sql).is_err(), "must reject {sql:?}");
    }
}

#[test]
fn row_number_rejects_conflicting_window_specifications() {
    let conflicting = [
        "SELECT ROW_NUMBER() OVER (), ROW_NUMBER() OVER (PARTITION BY id) FROM events",
        "SELECT ROW_NUMBER() OVER (PARTITION BY id), ROW_NUMBER() OVER (PARTITION BY other) FROM events",
        "SELECT ROW_NUMBER() OVER (PARTITION BY id), ROW_NUMBER() OVER (ORDER BY id ASC) FROM events",
    ];

    for sql in conflicting {
        assert!(sql::parse(sql).is_err(), "must reject {sql:?}");
    }
    assert!(
        sql::parse(
            "SELECT ROW_NUMBER() OVER (PARTITION BY id), \
                    ROW_NUMBER() OVER (PARTITION BY ID) FROM events"
        )
        .is_ok(),
        "identifiers in matching window specifications are case-insensitive"
    );
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
             SELECT ROW_NUMBER() OVER (ORDER BY id DESC) FROM events;",
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
             SELECT id, ROW_NUMBER() OVER (ORDER BY id ASC) FROM events;",
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
fn partitioned_row_number_enforces_group_and_key_caps_before_limit() {
    let setup = "CREATE TABLE events (id Int64, partition_key Int64); \
        INSERT INTO events VALUES (1, 10), (2, 20), (3, 10), (4, 30);";
    let key_cells = 3;
    let key_bytes = key_cells * ESTIMATED_GROUP_KEY_CELL_BYTES;
    let exact_limits = QueryResultLimits {
        max_groups: 3,
        max_group_key_cells: key_cells,
        max_group_key_bytes: key_bytes,
        ..QueryResultLimits::default()
    };
    let query_sql = "SELECT ROW_NUMBER() OVER (PARTITION BY partition_key) \
        FROM events LIMIT 0";

    let mut exact = Database::with_query_result_limits(exact_limits);
    exact.execute(setup).expect("setup");
    let results = exact.execute(query_sql).expect("exact caps succeed");
    assert!(
        query(&results[0]).rows.is_empty(),
        "LIMIT does not prevent exact partition working limits from succeeding"
    );

    let mut group_limited = Database::with_query_result_limits(QueryResultLimits {
        max_groups: 2,
        ..exact_limits
    });
    group_limited.execute(setup).expect("setup");
    assert_eq!(
        group_limited
            .execute(query_sql)
            .expect_err("the hidden third partition exceeds the group cap"),
        Error::ResourceLimitExceeded {
            resource: "SELECT groups",
            actual: 3,
            max: 2,
        }
    );

    let mut cell_limited = Database::with_query_result_limits(QueryResultLimits {
        max_group_key_cells: key_cells - 1,
        ..exact_limits
    });
    cell_limited.execute(setup).expect("setup");
    assert_eq!(
        cell_limited
            .execute(query_sql)
            .expect_err("the hidden third partition exceeds the key-cell cap"),
        Error::ResourceLimitExceeded {
            resource: "SELECT group key cells",
            actual: key_cells,
            max: key_cells - 1,
        }
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_group_key_bytes: key_bytes - 1,
        ..exact_limits
    });
    byte_limited.execute(setup).expect("setup");
    assert_eq!(
        byte_limited
            .execute(query_sql)
            .expect_err("the hidden third partition exceeds the key-byte cap"),
        Error::ResourceLimitExceeded {
            resource: "SELECT group key bytes",
            actual: key_bytes,
            max: key_bytes - 1,
        }
    );
}

#[test]
fn row_number_is_rendered_as_typed_csv_and_json() {
    let input = b"CREATE TABLE events (id Int64, rank_key Int64); \
        INSERT INTO events VALUES (7, 2), (9, 1), (8, 2); \
        SELECT id, ROW_NUMBER() OVER (PARTITION BY rank_key) AS sequence FROM events;";

    let mut csv = Vec::new();
    run_csv_batch(&input[..], &mut csv).expect("CSV output succeeds");
    assert_eq!(csv, b"id,sequence\n7,1\n9,1\n8,2\n");

    let mut json = Vec::new();
    run_json_batch(&input[..], &mut json).expect("JSON output succeeds");
    assert_eq!(
        json,
        concat!(
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"sequence","type":"Int64"}],"rows":[[7,1],[9,1],[8,2]]}"#,
            "\n"
        )
        .as_bytes()
    );
}
