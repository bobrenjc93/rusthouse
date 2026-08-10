use std::mem::size_of;

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
fn parses_bounded_column_minus_signed_int64_projections_and_ordering() {
    let statements = parse(
        "SELECT reading - -9223372036854775808, reading - +7 AS adjusted \
         FROM samples WHERE reading > 0 ORDER BY reading - +7 DESC LIMIT 2",
    )
    .expect("valid subtraction projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Int64Subtract {
                name: "reading".to_owned(),
                literal: i64::MIN,
                alias: None,
            },
            SelectItem::Int64Subtract {
                name: "reading".to_owned(),
                literal: 7,
                alias: Some("adjusted".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "reading - 7");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT reading - 1 FROM samples", limits)
        .expect("one subtraction item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT reading - 1, reading FROM samples", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn subtracts_negative_literals_and_integer_extremes_with_aliases() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES \
             (-9223372036854775808), (-7), (7), (9223372036854775807);",
        )
        .expect("setup");

    let minimum = query(
        &mut database,
        "SELECT reading - -9223372036854775808 FROM samples \
         WHERE reading = -9223372036854775808",
    );
    assert_eq!(
        minimum.columns,
        [ResultColumn {
            name: "reading - -9223372036854775808".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(minimum.rows, [vec![Value::Int64(0)]]);

    let maximum = query(
        &mut database,
        "SELECT reading - +9223372036854775807 AS distance FROM samples \
         WHERE reading = 9223372036854775807",
    );
    assert_eq!(maximum.columns[0].name, "distance");
    assert_eq!(maximum.rows, [vec![Value::Int64(0)]]);

    assert_eq!(
        query(
            &mut database,
            "SELECT reading - -5 AS adjusted FROM samples \
             WHERE reading > -9223372036854775808 \
             AND reading < 9223372036854775807 \
             ORDER BY adjusted LIMIT 2",
        )
        .rows,
        [vec![Value::Int64(-2)], vec![Value::Int64(12)]]
    );
}

#[test]
fn nullable_subtraction_propagates_nulls_and_preserves_projection_and_pagination() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Nullable(Int64)); \
             INSERT INTO samples VALUES \
             (NULL), (9223372036854775807), (-7), (NULL), (7), \
             (-9223372036854775808); \
             CREATE TABLE all_missing (reading Nullable(Int64)); \
             INSERT INTO all_missing VALUES (NULL), (NULL);",
        )
        .expect("setup");

    let paged = query(
        &mut database,
        "SELECT reading - 0 AS adjusted, reading - -1 AS shifted \
         FROM samples ORDER BY reading - 0 LIMIT 4 OFFSET 1",
    );
    assert_eq!(
        paged.columns,
        [
            ResultColumn {
                name: "adjusted".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "shifted".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        paged.rows,
        [
            vec![Value::Null(DataType::Int64), Value::Null(DataType::Int64)],
            vec![Value::Int64(i64::MIN), Value::Int64(i64::MIN + 1)],
            vec![Value::Int64(-7), Value::Int64(-6)],
            vec![Value::Int64(7), Value::Int64(8)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT reading - 0 AS adjusted FROM samples \
             ORDER BY adjusted DESC LIMIT 5",
        )
        .rows,
        [
            vec![Value::Int64(i64::MAX)],
            vec![Value::Int64(7)],
            vec![Value::Int64(-7)],
            vec![Value::Int64(i64::MIN)],
            vec![Value::Null(DataType::Int64)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT reading - -9223372036854775808 AS adjusted \
             FROM all_missing ORDER BY adjusted LIMIT 1 OFFSET 1",
        )
        .rows,
        [vec![Value::Null(DataType::Int64)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT reading - -9223372036854775808 FROM samples \
             WHERE reading = -9223372036854775808",
        )
        .rows,
        [vec![Value::Int64(0)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT reading - 9223372036854775807 FROM samples \
             WHERE reading = 9223372036854775807",
        )
        .rows,
        [vec![Value::Int64(0)]]
    );
    assert_eq!(
        database.execute(
            "SELECT reading - -1 FROM samples \
             WHERE reading = 9223372036854775807",
        ),
        Err(Error::NumericOverflow("Int64 subtraction".to_owned()))
    );
}

#[test]
fn checks_overflow_only_after_where_ordering_and_limit_select_rows() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE bounds (reading Int64); \
             INSERT INTO bounds VALUES \
             (0), (-9223372036854775808), (9223372036854775807);",
        )
        .expect("setup");

    assert!(
        query(&mut database, "SELECT reading - 1 FROM bounds LIMIT 0")
            .rows
            .is_empty()
    );
    assert_eq!(
        query(&mut database, "SELECT reading - 1 FROM bounds LIMIT 1").rows,
        [vec![Value::Int64(-1)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT reading - 1 AS adjusted FROM bounds \
             ORDER BY adjusted DESC LIMIT 1",
        )
        .rows,
        [vec![Value::Int64(i64::MAX - 1)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT reading - -1 FROM bounds \
             WHERE reading < 9223372036854775807 \
             ORDER BY reading - -1 DESC LIMIT 1",
        )
        .rows,
        [vec![Value::Int64(1)]]
    );

    for sql in [
        "SELECT reading - 1 FROM bounds WHERE reading = -9223372036854775808",
        "SELECT reading - -1 FROM bounds WHERE reading = 9223372036854775807",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::NumericOverflow("Int64 subtraction".to_owned())),
            "{sql}"
        );
    }
}

#[test]
fn rejects_unknown_non_int64_and_grouped_arguments_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT missing - 1 FROM samples"),
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
            database.execute(&format!("SELECT {name} - 1 FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("Int64 subtraction argument '{name}'"),
                expected: "Int64".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    for sql in [
        "SELECT i - 1 FROM samples GROUP BY i",
        "SELECT i - 1, COUNT(*) FROM samples GROUP BY i",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(
                "Int64 subtraction projections are only supported in ungrouped SELECT queries"
                    .to_owned()
            )),
            "{sql}"
        );
    }
}

#[test]
fn rejects_malformed_or_unsupported_subtraction_syntax() {
    for sql in [
        "SELECT reading - FROM samples",
        "SELECT reading - + FROM samples",
        "SELECT reading - NULL FROM samples",
        "SELECT reading - TRUE FROM samples",
        "SELECT reading - '1' FROM samples",
        "SELECT reading - 1.5 FROM samples",
        "SELECT reading - 1e2 FROM samples",
        "SELECT reading - (1) FROM samples",
        "SELECT reading - other FROM samples",
        "SELECT reading - 1 - 2 FROM samples",
        "SELECT reading - 1 adjusted FROM samples",
        "SELECT reading + 1 FROM samples",
        "SELECT reading - 9223372036854775808 FROM samples",
        "SELECT reading - -9223372036854775809 FROM samples",
        "SELECT reading - 1 FROM samples ORDER BY reading -",
        "SELECT reading - 1 FROM samples ORDER BY reading - other",
        "SELECT reading - 1 FROM samples ORDER BY reading - 1.5",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn subtraction_projection_obeys_result_caps() {
    let limits = QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT reading - 1 FROM samples LIMIT 2").rows,
        [vec![Value::Int64(0)], vec![Value::Int64(1)]]
    );
    assert_eq!(
        database.execute("SELECT reading - 1 FROM samples"),
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
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .expect("setup");
    assert_eq!(
        value_limited.execute("SELECT reading, reading - 1 FROM samples"),
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
        .execute("CREATE TABLE samples (reading Int64); INSERT INTO samples VALUES (1);")
        .expect("setup");
    assert!(matches!(
        byte_limited.execute("SELECT reading - 1 FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            max: 0,
            ..
        })
    ));

    let result_name = "adjusted";
    let exact_bytes = size_of::<ResultColumn>()
        + result_name.len()
        + 2 * size_of::<Vec<Value>>()
        + 2 * size_of::<Value>();
    let setup = "CREATE TABLE samples (reading Nullable(Int64)); \
                 INSERT INTO samples VALUES (NULL), (8);";
    let sql = "SELECT reading - 1 AS adjusted FROM samples";
    let exact_limits = QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
    exact.execute(setup).expect("setup");
    assert_eq!(
        query(&mut exact, sql).rows,
        [vec![Value::Null(DataType::Int64)], vec![Value::Int64(7)],]
    );

    let mut one_byte_short = Database::with_query_result_limits(QueryResultLimits {
        max_bytes: exact_bytes - 1,
        ..exact_limits
    });
    one_byte_short.execute(setup).expect("setup");
    assert_eq!(
        one_byte_short.execute(sql),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            actual: exact_bytes,
            max: exact_bytes - 1,
        })
    );
}

#[test]
fn emits_subtraction_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (reading Int64); \
               INSERT INTO samples VALUES (-2), (0), (5); \
               SELECT reading - -3 AS adjusted FROM samples ORDER BY adjusted;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+----------+\n\
         | adjusted |\n\
         +----------+\n\
         | 1        |\n\
         | 3        |\n\
         | 8        |\n\
         +----------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "adjusted\n1\n3\n8\n");

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(String::from_utf8(tsv).unwrap(), "adjusted\n1\n3\n8\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"adjusted\",\"type\":\"Int64\"}],\"rows\":[[1],[3],[8]]}\n"
    );
}
