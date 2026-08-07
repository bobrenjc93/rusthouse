use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, SelectItem, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::batch::{run_csv_batch, run_json_batch, run_table_batch, run_tsv_batch};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn parses_floor_as_a_bounded_select_item_with_an_optional_alias() {
    let statements = parse(
        "SELECT FLOOR(reading), floor(reading) AS whole FROM samples \
         WHERE reading < 10.0 ORDER BY FLOOR(reading) DESC LIMIT 2",
    )
    .expect("valid FLOOR projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Floor {
                name: "reading".to_owned(),
                alias: None,
            },
            SelectItem::Floor {
                name: "reading".to_owned(),
                alias: Some("whole".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "FLOOR(reading)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT FLOOR(reading) FROM samples", limits)
        .expect("one FLOOR item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT FLOOR(reading), reading FROM samples", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn floors_negative_fractional_and_integral_float64_values() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES \
             (-2.75), (-1.0), (-0.01), (0.0), (0.99), (1.0), (1.75);",
        )
        .expect("setup");

    let floored = query(&mut database, "SELECT FLOOR(reading) FROM samples");
    assert_eq!(
        floored.columns,
        [ResultColumn {
            name: "FLOOR(reading)".to_owned(),
            data_type: DataType::Float64,
        }]
    );
    assert_eq!(
        floored.rows,
        [
            vec![Value::Float64(-3.0)],
            vec![Value::Float64(-1.0)],
            vec![Value::Float64(-1.0)],
            vec![Value::Float64(0.0)],
            vec![Value::Float64(0.0)],
            vec![Value::Float64(1.0)],
            vec![Value::Float64(1.0)],
        ]
    );
    assert!(
        floored
            .rows
            .iter()
            .all(|row| { matches!(row.as_slice(), [Value::Float64(value)] if value.is_finite()) })
    );
}

#[test]
fn floors_large_finite_float64_values_without_narrowing() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES \
             (4503599627370495.5), (-4503599627370495.5), \
             (1.7976931348623157e308), (-1.7976931348623157e308);",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT FLOOR(reading) FROM samples").rows,
        [
            vec![Value::Float64(4_503_599_627_370_495.0)],
            vec![Value::Float64(-4_503_599_627_370_496.0)],
            vec![Value::Float64(f64::MAX)],
            vec![Value::Float64(-f64::MAX)],
        ]
    );
}

#[test]
fn filters_orders_by_floor_expression_or_alias_and_limits() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64, keep Bool); \
             INSERT INTO samples VALUES \
             (2.9, true), (-2.1, true), (1.2, true), (-1.0, true), (4.7, false);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT FLOOR(reading) FROM samples \
             WHERE keep = true ORDER BY FLOOR(reading) DESC LIMIT 3",
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
        "SELECT FLOOR(reading) AS whole FROM samples \
         WHERE keep = true ORDER BY whole LIMIT 2",
    );
    assert_eq!(aliased.columns[0].name, "whole");
    assert_eq!(
        aliased.rows,
        [vec![Value::Float64(-3.0)], vec![Value::Float64(-1.0)]]
    );
}

#[test]
fn rejects_unknown_non_float64_and_grouped_floor_inputs_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT FLOOR(missing) FROM samples"),
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
            database.execute(&format!("SELECT FLOOR({name}) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("FLOOR argument '{name}'"),
                expected: "Float64".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    for sql in [
        "SELECT FLOOR(f) FROM samples GROUP BY f",
        "SELECT FLOOR(f), COUNT(*) FROM samples GROUP BY f",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(
                "FLOOR projections are only supported in ungrouped SELECT queries".to_owned()
            )),
            "{sql}"
        );
    }
}

#[test]
fn rejects_malformed_floor_syntax() {
    for sql in [
        "SELECT FLOOR() FROM samples",
        "SELECT FLOOR(*) FROM samples",
        "SELECT FLOOR(1.5) FROM samples",
        "SELECT FLOOR('1.5') FROM samples",
        "SELECT FLOOR(reading, reading) FROM samples",
        "SELECT FLOOR(reading FROM samples",
        "SELECT FLOOR(reading) whole FROM samples",
        "SELECT FLOOR(FLOOR(reading)) FROM samples",
        "SELECT FLOOR(reading) FROM samples ORDER BY FLOOR()",
        "SELECT FLOOR(reading) FROM samples ORDER BY FLOOR(*)",
        "SELECT FLOOR(reading) FROM samples ORDER BY FLOOR(reading, reading)",
        "SELECT FLOOR(reading) FROM samples ORDER BY FLOOR(reading",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn floor_projection_obeys_result_caps() {
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
        query(&mut database, "SELECT FLOOR(reading) FROM samples LIMIT 2").rows,
        [vec![Value::Float64(1.0)], vec![Value::Float64(2.0)]]
    );
    assert_eq!(
        database.execute("SELECT FLOOR(reading) FROM samples"),
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
        value_limited.execute("SELECT FLOOR(reading), FLOOR(reading) FROM samples"),
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
        byte_limited.execute("SELECT FLOOR(reading) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            max: 0,
            ..
        })
    ));
}

#[test]
fn emits_floor_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (reading Float64); \
               INSERT INTO samples VALUES (-2.5), (0.5), (1.9); \
               SELECT FLOOR(reading) AS whole FROM samples ORDER BY whole;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+-------+\n\
         | whole |\n\
         +-------+\n\
         | -3.0  |\n\
         | 0.0   |\n\
         | 1.0   |\n\
         +-------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "whole\n-3.0\n0.0\n1.0\n");

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(String::from_utf8(tsv).unwrap(), "whole\n-3.0\n0.0\n1.0\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"whole\",\"type\":\"Float64\"}],\"rows\":[[-3.0],[0.0],[1.0]]}\n"
    );
}
