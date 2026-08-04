use rusthouse::batch::engine::{
    Database, ESTIMATED_GROUP_KEY_CELL_BYTES, QueryResult, QueryResultLimits, ResultColumn,
    StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::run_csv_batch;
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
fn parses_only_the_bounded_unaliased_column_list_distinct_shape() {
    for (sql, expected_columns) in [
        ("SELECT DISTINCT value FROM samples", vec!["value"]),
        (
            "select distinct Value, Other from Samples limit 0;",
            vec!["Value", "Other"],
        ),
    ] {
        let statements = parse(sql).expect("valid DISTINCT query");
        let Statement::Select(select) = &statements[0] else {
            panic!("expected SELECT");
        };
        assert!(select.distinct);
        let columns = select
            .items
            .iter()
            .map(|item| match item {
                SelectItem::Column { name, alias: None } => name.as_str(),
                _ => panic!("DISTINCT items must be unaliased columns"),
            })
            .collect::<Vec<_>>();
        assert_eq!(columns, expected_columns);
    }

    for sql in [
        "SELECT DISTINCT * FROM samples",
        "SELECT DISTINCT value AS renamed FROM samples",
        "SELECT DISTINCT value, other AS renamed FROM samples",
        "SELECT DISTINCT CAST(value AS Float64) FROM samples",
        "SELECT DISTINCT COUNT(value) FROM samples",
        "SELECT DISTINCT value FROM samples WHERE value = 1",
        "SELECT DISTINCT value FROM samples GROUP BY value",
        "SELECT DISTINCT value FROM samples HAVING value = 1",
        "SELECT DISTINCT value FROM samples ORDER BY value",
        "SELECT DISTINCT value FROM samples LIMIT -1",
    ] {
        assert!(
            matches!(parse(sql), Err(Error::Sql { .. })),
            "{sql:?} must return a typed SQL error"
        );
    }

    parse_with_limits(
        "SELECT DISTINCT value, other FROM samples",
        BatchSqlLimits {
            max_ast_list_items: 2,
            ..BatchSqlLimits::default()
        },
    )
    .expect("two DISTINCT projections fit the exact AST limit");
    assert_eq!(
        parse_with_limits(
            "SELECT DISTINCT value, other FROM samples",
            BatchSqlLimits {
                max_ast_list_items: 1,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn preserves_keyword_like_schema_identifiers_contextually() {
    let parser_cases = [
        ("SELECT DISTINCT v FROM limit", true),
        ("SELECT DISTINCT from FROM t", true),
        ("SELECT DISTINCT limit FROM t", true),
        ("SELECT DISTINCT distinct FROM t", true),
        ("SELECT distinct FROM t", false),
    ];
    for (sql, expected_distinct) in parser_cases {
        let statements = parse(sql).expect("keyword-like identifier is contextual");
        let Statement::Select(select) = &statements[0] else {
            panic!("expected SELECT");
        };
        assert_eq!(select.distinct, expected_distinct, "{sql:?}");
    }

    parse_with_limits(
        "SELECT distinct FROM t",
        BatchSqlLimits {
            max_ast_list_items: 1,
            ..BatchSqlLimits::default()
        },
    )
    .expect("a failed DISTINCT attempt must restore the AST allocation count");
    assert_eq!(
        parse_with_limits(
            "SELECT distinct, other FROM t",
            BatchSqlLimits {
                max_ast_list_items: 1,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE limit (v Int64); \
             INSERT INTO limit VALUES (2), (1), (2); \
             CREATE TABLE t (from String, distinct Int64, limit Bool); \
             INSERT INTO t VALUES \
             ('beta', 7, true), ('alpha', 7, false), ('beta', 9, true);",
        )
        .expect("keyword-like schema setup");

    assert_eq!(
        query(&mut database, "SELECT DISTINCT v FROM limit").rows,
        [vec![Value::Int64(2)], vec![Value::Int64(1)]]
    );
    assert_eq!(
        query(&mut database, "SELECT DISTINCT from FROM t").rows,
        [
            vec![Value::String("beta".to_owned())],
            vec![Value::String("alpha".to_owned())]
        ]
    );
    assert_eq!(
        query(&mut database, "SELECT DISTINCT limit FROM t").rows,
        [vec![Value::Bool(true)], vec![Value::Bool(false)]]
    );
    assert_eq!(
        query(&mut database, "SELECT distinct FROM t").rows,
        [
            vec![Value::Int64(7)],
            vec![Value::Int64(7)],
            vec![Value::Int64(9)]
        ]
    );
}

#[test]
fn deduplicates_all_physical_types_in_first_seen_order() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES \
             (2, 2.5, true, 'beta'), \
             (1, -1.0, false, 'alpha'), \
             (2, 2.5, true, 'beta'), \
             (3, 4.0, false, 'gamma'), \
             (1, -1.0, true, 'alpha');",
        )
        .expect("setup");

    let cases = [
        (
            "i",
            DataType::Int64,
            vec![Value::Int64(2), Value::Int64(1), Value::Int64(3)],
        ),
        (
            "f",
            DataType::Float64,
            vec![
                Value::Float64(2.5),
                Value::Float64(-1.0),
                Value::Float64(4.0),
            ],
        ),
        (
            "b",
            DataType::Bool,
            vec![Value::Bool(true), Value::Bool(false)],
        ),
        (
            "s",
            DataType::String,
            vec![
                Value::String("beta".to_owned()),
                Value::String("alpha".to_owned()),
                Value::String("gamma".to_owned()),
            ],
        ),
    ];

    for (name, data_type, values) in cases {
        let result = query(
            &mut database,
            &format!("SELECT DISTINCT {name} FROM samples"),
        );
        assert_eq!(
            result.columns,
            [ResultColumn {
                name: name.to_owned(),
                data_type,
            }]
        );
        assert_eq!(
            result.rows,
            values
                .into_iter()
                .map(|value| vec![value])
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn deduplicates_mixed_type_tuples_in_first_seen_order() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES \
             (2, 2.5, true, 'beta'), \
             (1, -1.0, false, 'alpha'), \
             (2, 2.5, true, 'beta'), \
             (3, 4.0, false, 'gamma'), \
             (1, -1.0, true, 'alpha'), \
             (1, -1.0, false, 'alpha');",
        )
        .expect("setup");

    let result = query(
        &mut database,
        "SELECT DISTINCT s, i, f, b FROM samples LIMIT 4",
    );
    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "s".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "i".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "f".to_owned(),
                data_type: DataType::Float64,
            },
            ResultColumn {
                name: "b".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![
                Value::String("beta".to_owned()),
                Value::Int64(2),
                Value::Float64(2.5),
                Value::Bool(true),
            ],
            vec![
                Value::String("alpha".to_owned()),
                Value::Int64(1),
                Value::Float64(-1.0),
                Value::Bool(false),
            ],
            vec![
                Value::String("gamma".to_owned()),
                Value::Int64(3),
                Value::Float64(4.0),
                Value::Bool(false),
            ],
            vec![
                Value::String("alpha".to_owned()),
                Value::Int64(1),
                Value::Float64(-1.0),
                Value::Bool(true),
            ],
        ]
    );
}

#[test]
fn rejects_unknown_and_duplicate_tuple_columns_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (value Int64, label String)")
        .expect("setup");

    assert_eq!(
        database
            .execute("SELECT DISTINCT value, missing FROM samples")
            .expect_err("unknown tuple columns are rejected"),
        Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        }
    );
    assert_eq!(
        database
            .execute("SELECT DISTINCT value, VALUE FROM samples")
            .expect_err("one physical column cannot appear twice"),
        Error::InvalidQuery("DISTINCT column 'VALUE' is listed more than once".to_owned())
    );
}

#[test]
fn handles_empty_input_and_zero_exact_and_exceeded_limits() {
    let mut empty = Database::with_query_result_limits(QueryResultLimits {
        max_groups: 0,
        ..QueryResultLimits::default()
    });
    empty
        .execute("CREATE TABLE samples (value Int64, label String)")
        .expect("setup");
    assert!(
        query(&mut empty, "SELECT DISTINCT value, label FROM samples")
            .rows
            .is_empty()
    );

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (2), (1), (2), (3), (1);",
        )
        .expect("setup");

    assert!(
        query(&mut database, "SELECT DISTINCT value FROM samples LIMIT 0")
            .rows
            .is_empty()
    );
    assert_eq!(
        query(&mut database, "SELECT DISTINCT value FROM samples LIMIT 3").rows,
        [
            vec![Value::Int64(2)],
            vec![Value::Int64(1)],
            vec![Value::Int64(3)]
        ]
    );
    assert_eq!(
        query(&mut database, "SELECT DISTINCT value FROM samples LIMIT 10").rows,
        [
            vec![Value::Int64(2)],
            vec![Value::Int64(1)],
            vec![Value::Int64(3)]
        ]
    );
}

#[test]
fn enforces_group_cap_before_limit_and_result_cap_after_limit() {
    let setup = "CREATE TABLE samples (value Int64, label String); \
        INSERT INTO samples VALUES (1, 'a'), (2, 'b'), (1, 'a'), (3, 'c');";
    let mut group_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: usize::MAX,
        max_values: usize::MAX,
        max_bytes: usize::MAX,
        max_groups: 2,
        ..QueryResultLimits::default()
    });
    group_limited.execute(setup).expect("setup");
    assert_eq!(
        group_limited
            .execute("SELECT DISTINCT value, label FROM samples LIMIT 0")
            .expect_err("LIMIT cannot bypass DISTINCT working-state limits"),
        Error::ResourceLimitExceeded {
            resource: "SELECT groups",
            actual: 3,
            max: 2,
        }
    );

    let mut result_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 4,
        max_bytes: usize::MAX,
        max_groups: 3,
        ..QueryResultLimits::default()
    });
    result_limited.execute(setup).expect("setup");
    assert_eq!(
        query(
            &mut result_limited,
            "SELECT DISTINCT value, label FROM samples LIMIT 2"
        )
        .rows,
        [
            vec![Value::Int64(1), Value::String("a".to_owned())],
            vec![Value::Int64(2), Value::String("b".to_owned())]
        ]
    );
    assert_eq!(
        result_limited
            .execute("SELECT DISTINCT value, label FROM samples")
            .expect_err("three output rows exceed the result cap"),
        Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        }
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 3,
        max_bytes: usize::MAX,
        max_groups: 3,
        ..QueryResultLimits::default()
    });
    value_limited.execute(setup).expect("setup");
    assert_eq!(
        value_limited
            .execute("SELECT DISTINCT value, label FROM samples LIMIT 2")
            .expect_err("two tuple rows exceed a three-value result cap"),
        Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 4,
            max: 3,
        }
    );
}

#[test]
fn enforces_group_key_cell_and_byte_caps_before_limit() {
    let setup = "CREATE TABLE samples (value Int64, label String, active Bool); \
        INSERT INTO samples VALUES \
        (1, 'a', true), (2, 'b', false), (1, 'a', true), (3, 'c', true);";
    let key_cells = 9;
    let key_bytes = key_cells * ESTIMATED_GROUP_KEY_CELL_BYTES;
    let exact_limits = QueryResultLimits {
        max_groups: 3,
        max_group_key_cells: key_cells,
        max_group_key_bytes: key_bytes,
        ..QueryResultLimits::default()
    };

    let mut exact = Database::with_query_result_limits(exact_limits);
    exact.execute(setup).expect("setup");
    assert!(
        query(
            &mut exact,
            "SELECT DISTINCT value, label, active FROM samples LIMIT 0"
        )
        .rows
        .is_empty(),
        "exact group-key limits allow the query even though LIMIT hides every group"
    );

    let mut cell_limited = Database::with_query_result_limits(QueryResultLimits {
        max_group_key_cells: key_cells - 1,
        ..exact_limits
    });
    cell_limited.execute(setup).expect("setup");
    assert_eq!(
        cell_limited
            .execute("SELECT DISTINCT value, label, active FROM samples LIMIT 0")
            .expect_err("the third tuple exceeds the group-key cell limit"),
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
            .execute("SELECT DISTINCT value, label, active FROM samples LIMIT 0")
            .expect_err("the third tuple exceeds the group-key byte limit"),
        Error::ResourceLimitExceeded {
            resource: "SELECT group key bytes",
            actual: key_bytes,
            max: key_bytes - 1,
        }
    );
}

#[test]
fn csv_batch_emits_distinct_tuples_with_escaping() {
    let input = b"CREATE TABLE labels (label String, active Bool); \
        INSERT INTO labels VALUES \
        ('beta', true), ('comma,value', false), ('beta', true), \
        ('beta', false), ('alpha', true); \
        SELECT DISTINCT label, active FROM labels LIMIT 4;";
    let mut output = Vec::new();

    run_csv_batch(&input[..], &mut output).expect("CSV batch succeeds");

    assert_eq!(
        output,
        b"label,active\nbeta,true\n\"comma,value\",false\nbeta,false\nalpha,true\n"
    );
}
