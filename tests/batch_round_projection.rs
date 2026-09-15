use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, SelectItem, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::batch::{run_csv_batch, run_json_batch};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn parses_round_as_a_bounded_select_item_with_an_optional_alias() {
    let statements = parse(
        "SELECT ROUND(reading), round(reading) AS nearest FROM samples \
         WHERE reading < 10.0 ORDER BY ROUND(reading) DESC LIMIT 2",
    )
    .expect("valid ROUND projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Round {
                name: "reading".to_owned(),
                alias: None,
            },
            SelectItem::Round {
                name: "reading".to_owned(),
                alias: Some("nearest".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "ROUND(reading)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT ROUND(reading) FROM samples", limits)
        .expect("one ROUND item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT ROUND(reading), reading FROM samples", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn rounds_halfway_values_to_even_and_preserves_signed_zero() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES \
             (-3.5), (-2.5), (-1.5), (-0.5), (-0.49), (-0.0), \
             (0.0), (0.49), (0.5), (1.5), (2.5), (3.5);",
        )
        .expect("setup");

    let rounded = query(&mut database, "SELECT ROUND(reading) FROM samples");
    assert_eq!(
        rounded.columns,
        [ResultColumn {
            name: "ROUND(reading)".to_owned(),
            data_type: DataType::Float64,
        }]
    );
    let expected: [f64; 12] = [
        -4.0, -2.0, -2.0, -0.0, -0.0, -0.0, 0.0, 0.0, 0.0, 2.0, 2.0, 4.0,
    ];
    assert_eq!(rounded.rows.len(), expected.len());
    for (row, expected) in rounded.rows.iter().zip(expected) {
        let [Value::Float64(actual)] = row.as_slice() else {
            panic!("ROUND returns one Float64");
        };
        assert_eq!(
            actual.to_bits(),
            expected.to_bits(),
            "{actual} != {expected}"
        );
    }
}

#[test]
fn preserves_large_finite_float64_values() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES \
             (4503599627370494.5), (-4503599627370494.5), \
             (4503599627370495.5), (-4503599627370495.5), \
             (1.7976931348623157e308), (-1.7976931348623157e308);",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT ROUND(reading) FROM samples").rows,
        [
            vec![Value::Float64(4_503_599_627_370_494.0)],
            vec![Value::Float64(-4_503_599_627_370_494.0)],
            vec![Value::Float64(4_503_599_627_370_496.0)],
            vec![Value::Float64(-4_503_599_627_370_496.0)],
            vec![Value::Float64(f64::MAX)],
            vec![Value::Float64(-f64::MAX)],
        ]
    );
}

#[test]
fn filters_orders_by_expression_or_alias_and_limits_before_projection() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64, keep Bool); \
             INSERT INTO samples VALUES \
             (2.5, true), (-2.5, true), (1.49, true), (-1.49, true), (4.5, false);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT ROUND(reading) FROM samples \
             WHERE keep = true ORDER BY ROUND(reading) DESC LIMIT 3",
        )
        .rows,
        [
            vec![Value::Float64(2.0)],
            vec![Value::Float64(1.0)],
            vec![Value::Float64(-1.0)],
        ]
    );

    let aliased = query(
        &mut database,
        "SELECT ROUND(reading) AS nearest FROM samples \
         WHERE keep = true ORDER BY nearest LIMIT 2",
    );
    assert_eq!(aliased.columns[0].name, "nearest");
    assert_eq!(
        aliased.rows,
        [vec![Value::Float64(-2.0)], vec![Value::Float64(-1.0)],]
    );
}

#[test]
fn ordering_by_rounded_expression_or_alias_preserves_ties_with_and_without_limit() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, reading Float64); \
             INSERT INTO samples VALUES \
             (40, 2.5), (10, -1.5), (30, 1.5), (20, -2.5), \
             (60, 0.5), (50, -0.5), (80, 2.49), (70, -2.49);",
        )
        .expect("setup");

    for order_by in ["ROUND(reading)", "nearest"] {
        for (direction, expected) in [
            (
                "ASC",
                [
                    (10, -2.0),
                    (20, -2.0),
                    (70, -2.0),
                    (60, 0.0),
                    (50, -0.0),
                    (40, 2.0),
                    (30, 2.0),
                    (80, 2.0),
                ],
            ),
            (
                "DESC",
                [
                    (40, 2.0),
                    (30, 2.0),
                    (80, 2.0),
                    (60, 0.0),
                    (50, -0.0),
                    (10, -2.0),
                    (20, -2.0),
                    (70, -2.0),
                ],
            ),
        ] {
            let expected =
                expected.map(|(id, rounded)| vec![Value::Int64(id), Value::Float64(rounded)]);
            for (limit, row_count) in [("", expected.len()), (" LIMIT 4", 4)] {
                let sql = format!(
                    "SELECT id, ROUND(reading) AS nearest FROM samples \
                     ORDER BY {order_by} {direction}{limit}"
                );
                assert_eq!(
                    query(&mut database, &sql).rows,
                    expected[..row_count],
                    "{sql}"
                );
            }
        }
    }
}

#[test]
fn rejects_unknown_non_float64_and_grouped_round_inputs_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT ROUND(missing) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );

    for (name, actual) in [
        ("i", DataType::Int64),
        ("b", DataType::Bool),
        ("s", DataType::String),
    ] {
        assert_eq!(
            database.execute(&format!("SELECT ROUND({name}) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("ROUND argument '{name}'"),
                expected: "Float64".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    for sql in [
        "SELECT ROUND(f) FROM samples GROUP BY f",
        "SELECT ROUND(f), COUNT(*) FROM samples GROUP BY f",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(
                "ROUND projections are only supported in ungrouped SELECT queries".to_owned()
            )),
            "{sql}"
        );
    }
}

#[test]
fn rejects_malformed_round_syntax() {
    for sql in [
        "SELECT ROUND() FROM samples",
        "SELECT ROUND(*) FROM samples",
        "SELECT ROUND(1.5) FROM samples",
        "SELECT ROUND('1.5') FROM samples",
        "SELECT ROUND(reading, reading) FROM samples",
        "SELECT ROUND(reading FROM samples",
        "SELECT ROUND(reading) nearest FROM samples",
        "SELECT ROUND(ROUND(reading)) FROM samples",
        "SELECT ROUND(reading) FROM samples ORDER BY ROUND()",
        "SELECT ROUND(reading) FROM samples ORDER BY ROUND(*)",
        "SELECT ROUND(reading) FROM samples ORDER BY ROUND(reading",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn round_projection_obeys_result_caps() {
    let limits = QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES (1.2), (2.5), (3.8);",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT ROUND(reading) FROM samples LIMIT 2").rows,
        [vec![Value::Float64(1.0)], vec![Value::Float64(2.0)]]
    );
    assert_eq!(
        database.execute("SELECT ROUND(reading) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 3,
        max_values: 5,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    value_limited
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES (1.2), (2.5), (3.8);",
        )
        .expect("setup");
    assert_eq!(
        value_limited.execute("SELECT ROUND(reading), ROUND(reading) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 6,
            max: 5,
        })
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 3,
        max_values: 3,
        max_bytes: 0,
        ..QueryResultLimits::default()
    });
    byte_limited
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES (1.2);",
        )
        .expect("setup");
    assert!(matches!(
        byte_limited.execute("SELECT ROUND(reading) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            max: 0,
            ..
        })
    ));
}

#[test]
fn emits_round_as_float64_in_csv_and_json() {
    let sql = "CREATE TABLE samples (reading Float64); \
               INSERT INTO samples VALUES (-2.5), (0.5), (1.49); \
               SELECT ROUND(reading) AS nearest FROM samples ORDER BY nearest;";

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "nearest\n-2.0\n0.0\n1.0\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"nearest\",\"type\":\"Float64\"}],\"rows\":[[-2.0],[0.0],[1.0]]}\n"
    );
}
