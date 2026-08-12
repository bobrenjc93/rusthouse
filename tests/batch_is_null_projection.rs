use std::mem::size_of;

use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, SelectItem, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected exactly one query result");
    };
    result.clone()
}

#[test]
fn parses_case_insensitive_is_null_with_alias_filter_order_and_pagination() {
    let statements = parse(
        "SELECT isNull(reading), ISNULL(reading) AS missing FROM samples \
         WHERE reading IS NULL OR reading > 0 \
         ORDER BY isNull(reading) DESC, missing ASC LIMIT 2 OFFSET 1",
    )
    .expect("valid isNull projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::IsNull {
                name: "reading".to_owned(),
                alias: None,
            },
            SelectItem::IsNull {
                name: "reading".to_owned(),
                alias: Some("missing".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "isNull(reading)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.order_by[1].name, "missing");
    assert!(!select.order_by[1].descending);
    assert_eq!(select.limit, Some(2));
    assert_eq!(select.offset, Some(1));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT isNull(reading) FROM samples", limits)
        .expect("one isNull item fits the AST limit");
    assert_eq!(
        parse_with_limits("SELECT isNull(reading), reading FROM samples", limits,),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn projects_mixed_and_all_null_nullable_int64_values() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (value Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL), (7), (NULL), (-2), (9); \
             CREATE TABLE all_null (value Nullable(Int64)); \
             INSERT INTO all_null VALUES (NULL), (NULL);",
        )
        .expect("setup");

    let aliased = query(
        &mut database,
        "SELECT value, isNull(value) AS missing FROM readings \
         WHERE value IS NULL OR value >= 7 \
         ORDER BY missing DESC LIMIT 3 OFFSET 1",
    );
    assert_eq!(
        aliased.columns,
        [
            ResultColumn {
                name: "value".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "missing".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    );
    assert_eq!(
        aliased.rows,
        [
            vec![Value::Null(DataType::Int64), Value::Bool(true)],
            vec![Value::Int64(7), Value::Bool(false)],
            vec![Value::Int64(9), Value::Bool(false)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT isNull(value) FROM readings \
             ORDER BY isNull(value) LIMIT 3",
        )
        .rows,
        [
            vec![Value::Bool(false)],
            vec![Value::Bool(false)],
            vec![Value::Bool(false)],
        ]
    );

    let all_null = query(
        &mut database,
        "SELECT isNull(value) FROM all_null ORDER BY isNull(value) DESC",
    );
    assert_eq!(
        all_null.columns,
        [ResultColumn {
            name: "isNull(value)".to_owned(),
            data_type: DataType::Bool,
        }]
    );
    assert_eq!(
        all_null.rows,
        [vec![Value::Bool(true)], vec![Value::Bool(true)]]
    );
}

#[test]
fn returns_false_for_every_non_nullable_physical_type() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES \
             (1, 1.5, true, 'one'), (2, -0.5, false, '');",
        )
        .expect("setup");

    let result = query(
        &mut database,
        "SELECT isNull(i), isNull(f), isNull(b), isNull(s) FROM samples",
    );
    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "isNull(i)".to_owned(),
                data_type: DataType::Bool,
            },
            ResultColumn {
                name: "isNull(f)".to_owned(),
                data_type: DataType::Bool,
            },
            ResultColumn {
                name: "isNull(b)".to_owned(),
                data_type: DataType::Bool,
            },
            ResultColumn {
                name: "isNull(s)".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
            ],
            vec![
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
            ],
        ]
    );
}

#[test]
fn rejects_missing_grouped_aggregate_and_malformed_shapes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (value Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL), (1);",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT isNull(missing) FROM readings"),
        Err(Error::ColumnNotFound {
            table: "readings".to_owned(),
            column: "missing".to_owned(),
        })
    );
    for sql in [
        "SELECT isNull(value) FROM readings GROUP BY value",
        "SELECT isNull(value), COUNT(*) FROM readings",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(
                "isNull projections are only supported in ungrouped SELECT queries".to_owned(),
            )),
            "{sql}"
        );
    }

    for sql in [
        "SELECT isNull() FROM readings",
        "SELECT isNull(*) FROM readings",
        "SELECT isNull(value, value) FROM readings",
        "SELECT isNull(value FROM readings",
        "SELECT isNull(value) missing FROM readings",
        "SELECT isNull(isNull(value)) FROM readings",
        "SELECT isNull(value) FROM readings ORDER BY isNull()",
        "SELECT isNull(value) FROM readings ORDER BY isNull(*)",
        "SELECT isNull(value) FROM readings ORDER BY isNull(value",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn obeys_selected_result_and_retained_result_bounds() {
    let setup = "CREATE TABLE readings (value Nullable(Int64)); \
                 INSERT INTO readings VALUES (1), (NULL), (2);";
    let result_name = "missing";
    let exact_bytes = size_of::<ResultColumn>()
        + result_name.len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>();
    let sql = "SELECT isNull(value) AS missing FROM readings LIMIT 1 OFFSET 1";

    let exact_limits = QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
    exact.execute(setup).expect("setup");
    assert_eq!(query(&mut exact, sql).rows, [vec![Value::Bool(true)]]);
    assert_eq!(
        exact.execute("SELECT isNull(value) FROM readings"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 1,
        })
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 3,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    value_limited.execute(setup).expect("setup");
    assert_eq!(
        value_limited.execute("SELECT value, isNull(value) AS missing FROM readings LIMIT 2",),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 4,
            max: 3,
        })
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

    let mut retained = Database::new();
    retained.execute(setup).expect("setup");
    retained
        .execute_with_result_limit(sql, exact_bytes)
        .expect("exact retained-result bound succeeds");
    assert_eq!(
        retained.execute_with_result_limit(sql, exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );
}
