use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
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
fn parses_cast_as_a_bounded_select_item_with_an_optional_alias() {
    let statements = parse(
        "SELECT CAST(reading AS Float64), cast(reading as float64) AS converted \
         FROM samples WHERE reading < 0 LIMIT 2",
    )
    .expect("valid CAST projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Cast {
                name: "reading".to_owned(),
                target_type: DataType::Float64,
                alias: None,
            },
            SelectItem::Cast {
                name: "reading".to_owned(),
                target_type: DataType::Float64,
                alias: Some("converted".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.limit, Some(2));

    let statements =
        parse("SELECT CAST(ratio AS Int64) AS whole FROM samples").expect("valid inverse CAST");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    assert_eq!(
        select.items,
        [SelectItem::Cast {
            name: "ratio".to_owned(),
            target_type: DataType::Int64,
            alias: Some("whole".to_owned()),
        }]
    );

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT CAST(reading AS Float64) FROM samples", limits)
        .expect("one CAST item fits the limit");
    assert_eq!(
        parse_with_limits(
            "SELECT CAST(reading AS Float64), reading FROM samples",
            limits,
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn cast_of_a_null_named_column_remains_a_table_projection() {
    let statements = parse("SELECT CAST(NULL AS Int64) AS converted FROM samples")
        .expect("NULL remains a legal CAST column identifier");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected a table-backed SELECT");
    };
    assert_eq!(
        select.items,
        [SelectItem::Cast {
            name: "NULL".to_owned(),
            target_type: DataType::Int64,
            alias: Some("converted".to_owned()),
        }]
    );
    assert_eq!(select.table, "samples");

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (NULL Float64); \
             INSERT INTO samples VALUES (2.5);",
        )
        .expect("setup succeeds");
    let result = query(&mut database, "SELECT CAST(NULL AS Int64) FROM samples");
    assert_eq!(
        result.columns,
        [ResultColumn {
            name: "CAST(NULL AS Int64)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(result.rows, [vec![Value::Int64(2)]]);
}

#[test]
fn float64_to_int64_truncates_fractions_signs_and_boundary_values() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES \
             (-9223372036854775808.0), (9223372036854774784.0), \
             (-12.9), (7.9), (-0.9), (0.9);",
        )
        .expect("setup");

    let all = query(&mut database, "SELECT CAST(reading AS Int64) FROM samples");
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "CAST(reading AS Int64)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::Int64(i64::MIN)],
            vec![Value::Int64(9_223_372_036_854_774_784)],
            vec![Value::Int64(-12)],
            vec![Value::Int64(7)],
            vec![Value::Int64(0)],
            vec![Value::Int64(0)],
        ]
    );

    let filtered = query(
        &mut database,
        "SELECT CAST(reading AS Int64) AS converted FROM samples \
         WHERE reading > -13.0 AND reading < 10.0 \
         ORDER BY converted DESC LIMIT 3",
    );
    assert_eq!(
        filtered.columns,
        [ResultColumn {
            name: "converted".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        filtered.rows,
        [
            vec![Value::Int64(7)],
            vec![Value::Int64(0)],
            vec![Value::Int64(0)],
        ]
    );

    database
        .execute(
            "CREATE TABLE ordered (id Int64, reading Float64); \
             INSERT INTO ordered VALUES (1, 1.9), (2, 1.1), (3, -1.1), (4, -1.9);",
        )
        .expect("ordered setup");
    assert_eq!(
        query(
            &mut database,
            "SELECT id, CAST(reading AS Int64) AS converted \
             FROM ordered ORDER BY converted LIMIT 3",
        )
        .rows,
        [
            vec![Value::Int64(3), Value::Int64(-1)],
            vec![Value::Int64(4), Value::Int64(-1)],
            vec![Value::Int64(1), Value::Int64(1)],
        ]
    );
}

#[test]
fn float64_to_int64_rejects_only_selected_out_of_range_values() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES \
             (9223372036854775808.0), (-9223372036854777856.0), (1.75);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Int64) FROM samples \
             WHERE reading > -9223372036854775808.0 \
             AND reading < 9223372036854775808.0",
        )
        .rows,
        [vec![Value::Int64(1)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Int64) AS converted FROM samples \
             WHERE reading > 0.0 ORDER BY converted LIMIT 1",
        )
        .rows,
        [vec![Value::Int64(1)]]
    );

    for predicate in [
        "reading = 9223372036854775808.0",
        "reading = -9223372036854777856.0",
    ] {
        assert_eq!(
            database.execute(&format!(
                "SELECT CAST(reading AS Int64) FROM samples WHERE {predicate}"
            )),
            Err(Error::NumericOverflow("CAST(Float64 AS Int64)".to_owned())),
            "{predicate}"
        );
    }
}

#[test]
fn ordering_selects_below_range_casts_independent_of_source_order() {
    for values in [
        "(-9223372036854775808.0), (-9223372036854777856.0)",
        "(-9223372036854777856.0), (-9223372036854775808.0)",
    ] {
        let mut database = Database::new();
        database
            .execute(&format!(
                "CREATE TABLE samples (reading Float64); \
                 INSERT INTO samples VALUES {values};"
            ))
            .expect("setup");

        assert_eq!(
            database.execute(
                "SELECT CAST(reading AS Int64) AS converted \
                 FROM samples ORDER BY converted LIMIT 1"
            ),
            Err(Error::NumericOverflow("CAST(Float64 AS Int64)".to_owned())),
            "source values {values}"
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT CAST(reading AS Int64) AS converted \
                 FROM samples ORDER BY converted DESC LIMIT 1",
            )
            .rows,
            [vec![Value::Int64(i64::MIN)]],
            "source values {values}"
        );
    }
}

#[test]
fn projects_negative_values_and_integer_extremes_with_filters_aliases_and_limits() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES \
             (-9223372036854775808), (-9007199254740993), (-7), (0), \
             (9223372036854775807);",
        )
        .expect("setup");

    let extremes = query(
        &mut database,
        "SELECT CAST(reading AS Float64) FROM samples \
         WHERE reading = -9223372036854775808 OR reading = 9223372036854775807",
    );
    assert_eq!(
        extremes.columns,
        [ResultColumn {
            name: "CAST(reading AS Float64)".to_owned(),
            data_type: DataType::Float64,
        }]
    );
    assert_eq!(
        extremes.rows,
        [
            vec![Value::Float64(-9_223_372_036_854_775_808.0)],
            vec![Value::Float64(9_223_372_036_854_775_808.0)],
        ]
    );

    let filtered = query(
        &mut database,
        "SELECT CAST(reading AS Float64) AS converted FROM samples \
         WHERE reading < 0 ORDER BY converted DESC LIMIT 2",
    );
    assert_eq!(
        filtered.columns,
        [ResultColumn {
            name: "converted".to_owned(),
            data_type: DataType::Float64,
        }]
    );
    assert_eq!(
        filtered.rows,
        [
            vec![Value::Float64(-7.0)],
            vec![Value::Float64(-9_007_199_254_740_992.0)],
        ]
    );
}

#[test]
fn rejects_unknown_and_invalid_cast_inputs_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (f Float64, b Bool, s String, i Int64);")
        .expect("setup");

    assert_eq!(
        database.execute("SELECT CAST(missing AS Float64) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );

    for (name, actual) in [
        ("f", DataType::Float64),
        ("b", DataType::Bool),
        ("s", DataType::String),
    ] {
        assert_eq!(
            database.execute(&format!("SELECT CAST({name} AS Float64) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("CAST argument '{name}'"),
                expected: "Int64".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    assert_eq!(
        database.execute("SELECT CAST(missing AS Int64) FROM samples"),
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
            database.execute(&format!("SELECT CAST({name} AS Int64) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("CAST argument '{name}'"),
                expected: "Float64".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }
}

#[test]
fn rejects_malformed_or_unsupported_cast_syntax() {
    for sql in [
        "SELECT CAST() FROM samples",
        "SELECT CAST(* AS Float64) FROM samples",
        "SELECT CAST(reading Float64) FROM samples",
        "SELECT CAST(reading AS) FROM samples",
        "SELECT CAST(reading AS Missing) FROM samples",
        "SELECT CAST(reading AS Bool) FROM samples",
        "SELECT CAST(reading AS String) FROM samples",
        "SELECT CAST(reading AS Float64 FROM samples",
        "SELECT CAST(reading AS Float64) converted FROM samples",
        "SELECT CAST(CAST(reading AS Float64) AS Float64) FROM samples",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn cast_remains_an_ordinary_projection() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (reading Int64); INSERT INTO samples VALUES (1);")
        .expect("setup");

    assert_eq!(
        database.execute("SELECT CAST(reading AS Float64), COUNT(*) FROM samples GROUP BY reading"),
        Err(Error::InvalidQuery(
            "CAST projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
}

#[test]
fn emits_float64_to_int64_as_typed_csv_and_json() {
    let sql = "CREATE TABLE samples (reading Float64); \
               INSERT INTO samples VALUES (-7.9), (0.9), (12.4); \
               SELECT CAST(reading AS Int64) AS converted \
               FROM samples ORDER BY converted;";

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "converted\n-7\n0\n12\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"converted\",\"type\":\"Int64\"}],\"rows\":[[-7],[0],[12]]}\n"
    );
}
