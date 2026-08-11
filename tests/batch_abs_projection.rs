use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, SelectItem, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::batch::{
    run_csv_batch, run_json_batch, run_json_compact_each_row_batch, run_json_each_row_batch,
    run_table_batch, run_tsv_batch,
};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn parses_abs_as_a_bounded_select_item_with_an_optional_alias() {
    let statements = parse(
        "SELECT ABS(reading), abs(reading) AS magnitude FROM samples \
         WHERE reading < 0 ORDER BY ABS(reading) DESC LIMIT 2",
    )
    .expect("valid ABS projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Abs {
                name: "reading".to_owned(),
                alias: None,
            },
            SelectItem::Abs {
                name: "reading".to_owned(),
                alias: Some("magnitude".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "ABS(reading)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT ABS(reading) FROM samples", limits)
        .expect("one ABS item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT ABS(reading), reading FROM samples", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn projects_checked_int64_absolute_values_with_aliases_filters_and_limits() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES (-12), (0), (7), (-9223372036854775808);",
        )
        .expect("setup");

    let unaliased = query(
        &mut database,
        "SELECT ABS(reading) FROM samples WHERE reading = -12",
    );
    assert_eq!(
        unaliased.columns,
        [ResultColumn {
            name: "ABS(reading)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(unaliased.rows, [vec![Value::Int64(12)]]);

    let selected = query(
        &mut database,
        "SELECT ABS(reading) AS magnitude FROM samples \
         WHERE reading > -9223372036854775808 ORDER BY magnitude LIMIT 2",
    );
    assert_eq!(
        selected.columns,
        [ResultColumn {
            name: "magnitude".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        selected.rows,
        [vec![Value::Int64(0)], vec![Value::Int64(7)]]
    );
}

#[test]
fn nullable_int64_abs_propagates_null_and_preserves_ordering_and_deferred_overflow() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Nullable(Int64)); \
             INSERT INTO samples VALUES \
             (NULL), (-3), (2), (NULL), (3), (-1), (-9223372036854775808); \
             CREATE TABLE all_null (reading Nullable(Int64)); \
             INSERT INTO all_null VALUES (NULL), (NULL), (NULL);",
        )
        .expect("setup");

    let selected = query(
        &mut database,
        "SELECT reading, ABS(reading) AS magnitude FROM samples \
         WHERE reading IS NULL OR reading != -9223372036854775808 \
         ORDER BY ABS(reading) LIMIT 4 OFFSET 1",
    );
    assert_eq!(
        selected.columns,
        [
            ResultColumn {
                name: "reading".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "magnitude".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        selected.rows,
        [
            vec![Value::Null(DataType::Int64), Value::Null(DataType::Int64)],
            vec![Value::Int64(-1), Value::Int64(1)],
            vec![Value::Int64(2), Value::Int64(2)],
            vec![Value::Int64(-3), Value::Int64(3)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT ABS(reading) AS magnitude FROM samples \
             WHERE reading IS NULL OR reading != -9223372036854775808 \
             ORDER BY magnitude DESC",
        )
        .rows,
        [
            vec![Value::Int64(3)],
            vec![Value::Int64(3)],
            vec![Value::Int64(2)],
            vec![Value::Int64(1)],
            vec![Value::Null(DataType::Int64)],
            vec![Value::Null(DataType::Int64)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT ABS(reading) AS magnitude FROM samples ORDER BY magnitude LIMIT 6",
        )
        .rows,
        [
            vec![Value::Null(DataType::Int64)],
            vec![Value::Null(DataType::Int64)],
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
            vec![Value::Int64(3)],
        ]
    );
    assert_eq!(
        database.execute(
            "SELECT ABS(reading) AS magnitude FROM samples ORDER BY magnitude DESC LIMIT 1"
        ),
        Err(Error::NumericOverflow("ABS(Int64)".to_owned()))
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT ABS(reading) AS magnitude FROM all_null \
             ORDER BY magnitude DESC LIMIT 2 OFFSET 1",
        )
        .rows,
        [
            vec![Value::Null(DataType::Int64)],
            vec![Value::Null(DataType::Int64)],
        ]
    );
}

#[test]
fn projects_float64_fractions_and_canonicalizes_signed_zero() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES (-2.75), (-0.0), (0.0), (1.5);",
        )
        .expect("setup");

    let absolute = query(&mut database, "SELECT ABS(reading) FROM samples");
    assert_eq!(
        absolute.columns,
        [ResultColumn {
            name: "ABS(reading)".to_owned(),
            data_type: DataType::Float64,
        }]
    );
    assert_eq!(
        absolute.rows,
        [
            vec![Value::Float64(2.75)],
            vec![Value::Float64(0.0)],
            vec![Value::Float64(0.0)],
            vec![Value::Float64(1.5)],
        ]
    );
    for row in &absolute.rows[1..=2] {
        let Value::Float64(zero) = row[0] else {
            panic!("ABS(Float64) returns Float64");
        };
        assert_eq!(zero.to_bits(), 0.0_f64.to_bits());
    }
}

#[test]
fn preserves_float64_extreme_magnitudes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES \
             (-1.7976931348623157e308), (1.7976931348623157e308);",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT ABS(reading) FROM samples").rows,
        [
            vec![Value::Float64(f64::MAX)],
            vec![Value::Float64(f64::MAX)],
        ]
    );
}

#[test]
fn filters_orders_and_pages_float64_abs_with_an_alias() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64, keep Bool); \
             INSERT INTO samples VALUES \
             (-3.5, true), (-0.0, true), (2.25, true), (-1.5, false);",
        )
        .expect("setup");

    let selected = query(
        &mut database,
        "SELECT ABS(reading) AS magnitude FROM samples \
         WHERE keep = true ORDER BY ABS(reading) LIMIT 2 OFFSET 1",
    );
    assert_eq!(selected.columns[0].name, "magnitude");
    assert_eq!(
        selected.rows,
        [vec![Value::Float64(2.25)], vec![Value::Float64(3.5)]]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT ABS(reading) AS magnitude FROM samples \
             WHERE keep = true ORDER BY magnitude DESC LIMIT 1 OFFSET 1",
        )
        .rows,
        [vec![Value::Float64(2.25)]]
    );
}

#[test]
fn reports_overflow_only_when_int64_min_survives_row_selection() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES (0), (-7), (-9223372036854775808);",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT ABS(reading) FROM samples LIMIT 2").rows,
        [vec![Value::Int64(0)], vec![Value::Int64(7)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT ABS(reading) FROM samples ORDER BY ABS(reading) LIMIT 2",
        )
        .rows,
        [vec![Value::Int64(0)], vec![Value::Int64(7)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT ABS(reading) FROM samples WHERE reading != -9223372036854775808",
        )
        .rows,
        [vec![Value::Int64(0)], vec![Value::Int64(7)]]
    );

    for sql in [
        "SELECT ABS(reading) FROM samples WHERE reading = -9223372036854775808",
        "SELECT ABS(reading) FROM samples LIMIT 3",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::NumericOverflow("ABS(Int64)".to_owned())),
            "{sql}"
        );
    }
}

#[test]
fn rejects_unknown_non_numeric_and_grouped_abs_inputs_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT ABS(missing) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );

    for (name, actual) in [("b", DataType::Bool), ("s", DataType::String)] {
        assert_eq!(
            database.execute(&format!("SELECT ABS({name}) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("ABS argument '{name}'"),
                expected: "Int64 or Float64".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    for sql in [
        "SELECT ABS(i), COUNT(*) FROM samples GROUP BY i",
        "SELECT ABS(f), COUNT(*) FROM samples GROUP BY f",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(
                "ABS projections are only supported in ungrouped SELECT queries".to_owned()
            )),
            "{sql}"
        );
    }
}

#[test]
fn rejects_malformed_abs_syntax() {
    for sql in [
        "SELECT ABS() FROM samples",
        "SELECT ABS(*) FROM samples",
        "SELECT ABS(-1) FROM samples",
        "SELECT ABS('1') FROM samples",
        "SELECT ABS(reading, reading) FROM samples",
        "SELECT ABS(reading FROM samples",
        "SELECT ABS(reading) magnitude FROM samples",
        "SELECT ABS(ABS(reading)) FROM samples",
        "SELECT ABS(reading) FROM samples ORDER BY ABS()",
        "SELECT ABS(reading) FROM samples ORDER BY ABS(*)",
        "SELECT ABS(reading) FROM samples ORDER BY ABS(reading",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn abs_projection_obeys_result_caps() {
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
             INSERT INTO samples VALUES (-1.25), (-2.5), (-3.75);",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT ABS(reading) FROM samples LIMIT 2").rows,
        [vec![Value::Float64(1.25)], vec![Value::Float64(2.5)]]
    );
    assert_eq!(
        database.execute("SELECT ABS(reading) FROM samples"),
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
             INSERT INTO samples VALUES (-1.25), (-2.5), (-3.75);",
        )
        .expect("setup");
    assert_eq!(
        value_limited.execute("SELECT ABS(reading), ABS(reading) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 6,
            max: 5,
        })
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: 0,
        ..QueryResultLimits::default()
    });
    byte_limited
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES (-1.25);",
        )
        .expect("setup");
    assert!(matches!(
        byte_limited.execute("SELECT ABS(reading) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            max: 0,
            ..
        })
    ));

    let mut nullable_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    nullable_limited
        .execute(
            "CREATE TABLE samples (reading Nullable(Int64)); \
             INSERT INTO samples VALUES (NULL), (-2), (3);",
        )
        .expect("setup");
    assert_eq!(
        query(
            &mut nullable_limited,
            "SELECT ABS(reading) FROM samples LIMIT 2",
        )
        .rows,
        [vec![Value::Null(DataType::Int64)], vec![Value::Int64(2)],]
    );
    assert_eq!(
        nullable_limited.execute("SELECT ABS(reading) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );
}

#[test]
fn emits_float64_abs_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (reading Float64); \
               INSERT INTO samples VALUES (-2.5), (-0.0), (1.25); \
               SELECT ABS(reading) AS magnitude FROM samples ORDER BY magnitude;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+-----------+\n\
         | magnitude |\n\
         +-----------+\n\
         | 0.0       |\n\
         | 1.25      |\n\
         | 2.5       |\n\
         +-----------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(
        String::from_utf8(csv).unwrap(),
        "magnitude\n0.0\n1.25\n2.5\n"
    );

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(
        String::from_utf8(tsv).unwrap(),
        "magnitude\n0.0\n1.25\n2.5\n"
    );

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"magnitude\",\"type\":\"Float64\"}],\"rows\":[[0.0],[1.25],[2.5]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"magnitude\":0.0}\n{\"magnitude\":1.25}\n{\"magnitude\":2.5}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[0.0]\n[1.25]\n[2.5]\n"
    );
}
