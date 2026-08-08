use rusthouse::batch::engine::{
    Database, QueryResultLimits, ROW_NUMBER_ORDERING_STATE_ENTRY_BYTES, ResultColumn,
    StatementResult,
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
fn ordered_row_number_preflights_exact_filtered_state_and_preserves_stable_ties() {
    let setup = "CREATE TABLE events (id Int64, rank_key Int64, keep Bool); \
                 INSERT INTO events VALUES \
                     (1, 2, true), (2, 1, true), (3, 2, true), \
                     (4, 1, true), (5, 0, false);";
    let filtered_state_bytes = 4 * ROW_NUMBER_ORDERING_STATE_ENTRY_BYTES;
    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_ordering_state_bytes: filtered_state_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(setup).expect("setup");

    let results = exact
        .execute(
            "SELECT id, ROW_NUMBER() OVER (ORDER BY rank_key ASC) AS n \
             FROM events WHERE keep = true LIMIT 3; \
             SELECT id, ROW_NUMBER() OVER (ORDER BY rank_key DESC) AS n \
             FROM events WHERE keep = true LIMIT 3;",
        )
        .expect("the exact filtered-state boundary succeeds");
    assert_eq!(
        query(&results[0]).rows,
        [
            vec![Value::Int64(2), Value::Int64(1)],
            vec![Value::Int64(4), Value::Int64(2)],
            vec![Value::Int64(1), Value::Int64(3)],
        ]
    );
    assert_eq!(
        query(&results[1]).rows,
        [
            vec![Value::Int64(1), Value::Int64(1)],
            vec![Value::Int64(3), Value::Int64(2)],
            vec![Value::Int64(2), Value::Int64(3)],
        ]
    );

    assert_eq!(
        exact.execute(
            "SELECT ROW_NUMBER() OVER (ORDER BY rank_key ASC) \
             FROM events LIMIT 1"
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT ordering state bytes",
            actual: 5 * ROW_NUMBER_ORDERING_STATE_ENTRY_BYTES,
            max: filtered_state_bytes,
        })
    );

    let mut one_byte_short = Database::with_query_result_limits(QueryResultLimits {
        max_ordering_state_bytes: filtered_state_bytes - 1,
        ..QueryResultLimits::default()
    });
    one_byte_short.execute(setup).expect("setup");
    assert_eq!(
        one_byte_short.execute(
            "SELECT ROW_NUMBER() OVER (ORDER BY rank_key DESC) \
             FROM events WHERE keep = true LIMIT 1"
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT ordering state bytes",
            actual: filtered_state_bytes,
            max: filtered_state_bytes - 1,
        })
    );
}

#[test]
fn ordered_row_number_limit_zero_still_charges_state_but_empty_inputs_do_not() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_ordering_state_bytes: 0,
        ..QueryResultLimits::default()
    });
    let results = database
        .execute(
            "CREATE TABLE empty (rank_key Int64); \
             CREATE TABLE events (rank_key Int64, keep Bool); \
             INSERT INTO events VALUES (2, true), (1, false); \
             SELECT ROW_NUMBER() OVER (ORDER BY rank_key ASC) FROM empty LIMIT 0; \
             SELECT ROW_NUMBER() OVER (ORDER BY rank_key DESC) \
             FROM events WHERE keep = false AND rank_key = 2 LIMIT 0;",
        )
        .expect("empty filtered ordering state fits a zero-byte limit");
    assert!(query(&results[3]).rows.is_empty());
    assert!(query(&results[4]).rows.is_empty());

    assert_eq!(
        database.execute(
            "SELECT ROW_NUMBER() OVER (ORDER BY rank_key ASC) \
             FROM events WHERE keep = true LIMIT 0"
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT ordering state bytes",
            actual: ROW_NUMBER_ORDERING_STATE_ENTRY_BYTES,
            max: 0,
        })
    );

    let results = database
        .execute("SELECT ROW_NUMBER() OVER () FROM events LIMIT 0")
        .expect("unordered ROW_NUMBER does not use ordering state");
    assert!(query(&results[0]).rows.is_empty());
}

#[test]
fn ordered_row_number_limits_match_a_full_stable_ordering() {
    let source_rows = [
        (-2, -100),
        (10, 2),
        (20, 1),
        (30, 2),
        (40, 1),
        (50, 3),
        (-1, 100),
        (60, 3),
        (70, 1),
        (80, 2),
        (90, 3),
    ];
    let values = source_rows
        .iter()
        .map(|(id, rank_key)| format!("({id}, {rank_key})"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut database = Database::new();
    database
        .execute(&format!(
            "CREATE TABLE events (id Int64, rank_key Int64); \
             INSERT INTO events VALUES {values};"
        ))
        .expect("fixture succeeds");

    let matching_row_count = source_rows.iter().filter(|(id, _)| *id >= 0).count();
    let limits = [0, 2, matching_row_count, matching_row_count + 5];

    for (direction, descending) in [("ASC", false), ("DESC", true)] {
        let mut full_stable_order = source_rows
            .iter()
            .filter(|(id, _)| *id >= 0)
            .collect::<Vec<_>>();
        full_stable_order.sort_by(|left, right| {
            let comparison = left.1.cmp(&right.1);
            if descending {
                comparison.reverse()
            } else {
                comparison
            }
        });
        let full_result = full_stable_order
            .iter()
            .enumerate()
            .map(|(index, (id, rank_key))| {
                vec![
                    Value::Int64(*id),
                    Value::Int64(*rank_key),
                    Value::Int64(i64::try_from(index + 1).unwrap()),
                ]
            })
            .collect::<Vec<_>>();

        for limit in limits {
            let results = database
                .execute(&format!(
                    "SELECT id, rank_key, \
                            ROW_NUMBER() OVER (ORDER BY rank_key {direction}) AS n \
                     FROM events WHERE id >= 0 LIMIT {limit}"
                ))
                .expect("limited ordered ROW_NUMBER succeeds");
            assert_eq!(
                query(&results[0]).rows,
                full_result[..limit.min(full_result.len())],
                "{direction} LIMIT {limit}"
            );
        }
    }
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
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_ordering_state_bytes: 0,
        ..QueryResultLimits::default()
    });
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
fn row_number_rejects_arguments_and_unsupported_window_specifications() {
    let malformed = [
        "SELECT ROW_NUMBER(id) OVER () FROM events",
        "SELECT ROW_NUMBER(*) OVER () FROM events",
        "SELECT ROW_NUMBER(1) OVER () FROM events",
        "SELECT ROW_NUMBER() FROM events",
        "SELECT ROW_NUMBER() OVER window_name FROM events",
        "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM events",
        "SELECT ROW_NUMBER() OVER (ORDER BY id ASC, id DESC) FROM events",
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
fn row_number_is_rendered_as_typed_csv_and_json() {
    let input = b"CREATE TABLE events (id Int64, rank_key Int64); \
        INSERT INTO events VALUES (7, 2), (9, 1); \
        SELECT id, ROW_NUMBER() OVER (ORDER BY rank_key ASC) AS sequence FROM events;";

    let mut csv = Vec::new();
    run_csv_batch(&input[..], &mut csv).expect("CSV output succeeds");
    assert_eq!(csv, b"id,sequence\n9,1\n7,2\n");

    let mut json = Vec::new();
    run_json_batch(&input[..], &mut json).expect("JSON output succeeds");
    assert_eq!(
        json,
        concat!(
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"sequence","type":"Int64"}],"rows":[[9,1],[7,2]]}"#,
            "\n"
        )
        .as_bytes()
    );
}
