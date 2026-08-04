use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, SelectItem, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};

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
fn rejects_unknown_and_non_int64_cast_inputs_with_typed_errors() {
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
}

#[test]
fn rejects_malformed_or_unsupported_cast_syntax() {
    for sql in [
        "SELECT CAST() FROM samples",
        "SELECT CAST(* AS Float64) FROM samples",
        "SELECT CAST(reading Float64) FROM samples",
        "SELECT CAST(reading AS) FROM samples",
        "SELECT CAST(reading AS Missing) FROM samples",
        "SELECT CAST(reading AS Int64) FROM samples",
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
