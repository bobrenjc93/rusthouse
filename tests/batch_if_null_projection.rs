use std::mem::size_of;

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
fn parses_exact_if_null_shape_alias_ordering_and_signed_extrema() {
    let statements = parse(
        "SELECT ifNull(reading, -9223372036854775808), \
         IFNULL(reading, +7) AS filled FROM samples \
         WHERE reading IS NULL ORDER BY ifNull(reading, +7) DESC LIMIT 2 OFFSET 1",
    )
    .expect("valid ifNull projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::IfNullInt64 {
                name: "reading".to_owned(),
                fallback: i64::MIN,
                alias: None,
            },
            SelectItem::IfNullInt64 {
                name: "reading".to_owned(),
                fallback: 7,
                alias: Some("filled".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "ifNull(reading, 7)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));
    assert_eq!(select.offset, Some(1));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT ifNull(reading, 0) FROM samples", limits)
        .expect("one ifNull item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT ifNull(reading, 0), reading FROM samples", limits,),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn fills_mixed_and_all_null_rows_preserving_extrema_and_aliases() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Nullable(Int64)); \
             INSERT INTO samples VALUES \
             (NULL), (-9223372036854775808), (5), (NULL), \
             (9223372036854775807); \
             CREATE TABLE all_null (reading Nullable(Int64)); \
             INSERT INTO all_null VALUES (NULL), (NULL);",
        )
        .expect("setup");

    let extrema = query(
        &mut database,
        "SELECT ifNull(reading, +7) FROM samples \
         WHERE reading < -9223372036854775807 OR reading > 9223372036854775806",
    );
    assert_eq!(
        extrema.columns,
        [ResultColumn {
            name: "ifNull(reading, 7)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        extrema.rows,
        [vec![Value::Int64(i64::MIN)], vec![Value::Int64(i64::MAX)],]
    );

    let filled = query(
        &mut database,
        "SELECT reading, ifNull(reading, 4) AS filled FROM samples \
         WHERE reading IS NULL OR reading > 0 \
         ORDER BY ifNull(reading, +4) LIMIT 3 OFFSET 1",
    );
    assert_eq!(
        filled.columns,
        [
            ResultColumn {
                name: "reading".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "filled".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        filled.rows,
        [
            vec![Value::Null(DataType::Int64), Value::Int64(4)],
            vec![Value::Int64(5), Value::Int64(5)],
            vec![Value::Int64(i64::MAX), Value::Int64(i64::MAX)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT ifNull(reading, -9223372036854775808) AS filled \
             FROM all_null ORDER BY filled DESC LIMIT 1 OFFSET 1",
        )
        .rows,
        [vec![Value::Int64(i64::MIN)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT ifNull(reading, 9223372036854775807) AS filled \
             FROM all_null ORDER BY filled LIMIT 1",
        )
        .rows,
        [vec![Value::Int64(i64::MAX)]]
    );
}

#[test]
fn orders_by_if_null_alias_using_replaced_values_and_stable_ties() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Nullable(Int64)); \
             INSERT INTO samples VALUES (NULL), (5), (-2), (NULL), (9);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT ifNull(reading, 4) AS filled FROM samples \
             ORDER BY filled LIMIT 3 OFFSET 1",
        )
        .rows,
        [
            vec![Value::Int64(4)],
            vec![Value::Int64(4)],
            vec![Value::Int64(5)],
        ]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT reading, ifNull(reading, 10) FROM samples \
             ORDER BY ifNull(reading, 10) DESC LIMIT 3",
        )
        .rows,
        [
            vec![Value::Null(DataType::Int64), Value::Int64(10)],
            vec![Value::Null(DataType::Int64), Value::Int64(10)],
            vec![Value::Int64(9), Value::Int64(9)],
        ]
    );
}

#[test]
fn projects_grouped_keys_with_aggregates_extrema_collisions_and_pagination() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (value Nullable(Int64)); \
             INSERT INTO readings VALUES \
             (NULL), (0), (-9223372036854775808), (NULL), (0), \
             (9223372036854775807), (-5); \
             CREATE TABLE all_null (value Nullable(Int64)); \
             INSERT INTO all_null VALUES (NULL), (NULL), (NULL);",
        )
        .expect("setup");

    let grouped = query(
        &mut database,
        "SELECT ifNull(value, 0) AS filled, COUNT(*) AS rows, \
                MIN(value) AS minimum, MAX(value) AS maximum \
         FROM readings GROUP BY value HAVING rows >= 1 \
         ORDER BY filled",
    );
    assert_eq!(
        grouped.columns,
        [
            ResultColumn {
                name: "filled".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "rows".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "minimum".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "maximum".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        grouped.rows,
        [
            vec![
                Value::Int64(i64::MIN),
                Value::Int64(1),
                Value::Int64(i64::MIN),
                Value::Int64(i64::MIN),
            ],
            vec![
                Value::Int64(-5),
                Value::Int64(1),
                Value::Int64(-5),
                Value::Int64(-5),
            ],
            vec![
                Value::Int64(0),
                Value::Int64(2),
                Value::Null(DataType::Int64),
                Value::Null(DataType::Int64),
            ],
            vec![
                Value::Int64(0),
                Value::Int64(2),
                Value::Int64(0),
                Value::Int64(0),
            ],
            vec![
                Value::Int64(i64::MAX),
                Value::Int64(1),
                Value::Int64(i64::MAX),
                Value::Int64(i64::MAX),
            ],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT COUNT(*) AS rows, ifNull(value, 0) AS filled, MIN(value) AS minimum \
             FROM readings GROUP BY value HAVING rows >= 2 \
             ORDER BY ifNull(value, 0) LIMIT 1 OFFSET 1",
        )
        .rows,
        [vec![Value::Int64(2), Value::Int64(0), Value::Int64(0),]]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT ifNull(value, 0) AS filled FROM readings \
             GROUP BY value ORDER BY filled LIMIT 4 OFFSET 1",
        )
        .rows,
        [
            vec![Value::Int64(-5)],
            vec![Value::Int64(0)],
            vec![Value::Int64(0)],
            vec![Value::Int64(i64::MAX)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT ifNull(value, 9223372036854775807), COUNT(*) \
             FROM all_null GROUP BY value",
        )
        .rows,
        [vec![Value::Int64(i64::MAX), Value::Int64(3)]]
    );
}

#[test]
fn grouped_if_null_obeys_result_and_group_bounds() {
    let setup = "CREATE TABLE readings (value Nullable(Int64)); \
                 INSERT INTO readings VALUES (NULL), (0), (1);";
    let result_name = "filled";
    let exact_bytes = size_of::<ResultColumn>()
        + result_name.len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>();
    let paginated = "SELECT ifNull(value, 0) AS filled FROM readings \
                     GROUP BY value ORDER BY filled LIMIT 1";
    let exact_limits = QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
    exact.execute(setup).expect("setup");
    assert_eq!(query(&mut exact, paginated).rows, [vec![Value::Int64(0)]]);
    assert_eq!(
        exact.execute(
            "SELECT ifNull(value, 0) AS filled FROM readings GROUP BY value ORDER BY filled",
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 1,
        })
    );
    exact
        .execute_with_result_limit(paginated, exact_bytes)
        .expect("exact retained-result bound succeeds");
    assert_eq!(
        exact.execute_with_result_limit(paginated, exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );

    let mut group_limited = Database::with_query_result_limits(QueryResultLimits {
        max_groups: 2,
        ..QueryResultLimits::default()
    });
    group_limited.execute(setup).expect("setup");
    assert_eq!(
        group_limited.execute(paginated),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT groups",
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
    value_limited.execute(setup).expect("setup");
    assert_eq!(
        value_limited.execute("SELECT ifNull(value, 0), COUNT(*) FROM readings GROUP BY value",),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 6,
            max: 5,
        })
    );
}

#[test]
fn rejects_non_nullable_non_int64_ungrouped_sources_and_malformed_shapes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one'); \
             CREATE TABLE nullable (n Nullable(Int64)); \
             INSERT INTO nullable VALUES (NULL); \
             CREATE TABLE grouped_nullable \
                 (id Int64, n Nullable(Int64), other Nullable(Int64)); \
             INSERT INTO grouped_nullable VALUES (1, NULL, 1), (2, 2, 1);",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT ifNull(missing, 0) FROM nullable"),
        Err(Error::ColumnNotFound {
            table: "nullable".to_owned(),
            column: "missing".to_owned(),
        })
    );
    for (name, actual) in [
        ("i", "Int64"),
        ("f", "Float64"),
        ("b", "Bool"),
        ("s", "String"),
    ] {
        assert_eq!(
            database.execute(&format!("SELECT ifNull({name}, 0) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("ifNull first argument '{name}'"),
                expected: "Nullable(Int64)".to_owned(),
                actual: actual.to_owned(),
            }),
            "column {name}"
        );
    }

    for sql in [
        "SELECT ifNull(n, 0), COUNT(*) FROM nullable",
        "SELECT ifNull(n, 0), COUNT(*) FROM grouped_nullable GROUP BY other",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(
                "column 'n' must appear in GROUP BY".to_owned()
            )),
            "{sql}"
        );
    }

    for sql in [
        "SELECT ifNull(n, 0) FROM nullable GROUP BY ifNull(n, 0)",
        "SELECT ifNull() FROM nullable",
        "SELECT ifNull(n) FROM nullable",
        "SELECT ifNull(n,) FROM nullable",
        "SELECT ifNull(n, NULL) FROM nullable",
        "SELECT ifNull(n, TRUE) FROM nullable",
        "SELECT ifNull(n, '0') FROM nullable",
        "SELECT ifNull(n, 0.0) FROM nullable",
        "SELECT ifNull(n, other) FROM nullable",
        "SELECT ifNull(0, n) FROM nullable",
        "SELECT ifNull(n, 0, 1) FROM nullable",
        "SELECT ifNull(n, 0) filled FROM nullable",
        "SELECT ifNull(n, 9223372036854775808) FROM nullable",
        "SELECT ifNull(n, -9223372036854775809) FROM nullable",
        "SELECT ifNull(n, 0) FROM nullable ORDER BY ifNull(n)",
        "SELECT ifNull(n, 0) FROM nullable ORDER BY ifNull(n, other)",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn if_null_projection_obeys_exact_result_bounds() {
    let limits = QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE samples (reading Nullable(Int64)); \
             INSERT INTO samples VALUES (NULL), (2), (3);",
        )
        .expect("setup");
    assert_eq!(
        query(
            &mut database,
            "SELECT ifNull(reading, 0) FROM samples LIMIT 2",
        )
        .rows,
        [vec![Value::Int64(0)], vec![Value::Int64(2)]]
    );
    assert_eq!(
        database.execute("SELECT ifNull(reading, 0) FROM samples"),
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
            "CREATE TABLE samples (reading Nullable(Int64)); \
             INSERT INTO samples VALUES (NULL), (2), (3);",
        )
        .expect("setup");
    assert_eq!(
        value_limited.execute("SELECT reading, ifNull(reading, 0) AS filled FROM samples",),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 6,
            max: 5,
        })
    );

    let result_name = "filled";
    let exact_bytes = size_of::<ResultColumn>()
        + result_name.len()
        + 2 * size_of::<Vec<Value>>()
        + 2 * size_of::<Value>();
    let setup = "CREATE TABLE samples (reading Nullable(Int64)); \
                 INSERT INTO samples VALUES (NULL), (8);";
    let sql = "SELECT ifNull(reading, -1) AS filled FROM samples";
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
        [vec![Value::Int64(-1)], vec![Value::Int64(8)]]
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
fn emits_if_null_in_every_cli_output_format() {
    let sql = "CREATE TABLE samples (reading Nullable(Int64)); \
               INSERT INTO samples VALUES (NULL), (-2), (5); \
               SELECT ifNull(reading, 3) AS filled FROM samples ORDER BY filled;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+--------+\n\
         | filled |\n\
         +--------+\n\
         | -2     |\n\
         | 3      |\n\
         | 5      |\n\
         +--------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "filled\n-2\n3\n5\n");

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(String::from_utf8(tsv).unwrap(), "filled\n-2\n3\n5\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"filled\",\"type\":\"Int64\"}],\"rows\":[[-2],[3],[5]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"filled\":-2}\n{\"filled\":3}\n{\"filled\":5}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[-2]\n[3]\n[5]\n"
    );
}
