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
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected exactly one query result");
    };
    result.clone()
}

#[test]
fn parses_case_insensitive_is_not_null_with_alias_filter_order_and_pagination() {
    let statements = parse(
        "SELECT isNotNull(reading), ISNOTNULL(reading) AS present FROM samples \
         WHERE reading IS NULL OR reading > 0 \
         ORDER BY isNotNull(reading) DESC, present ASC LIMIT 2 OFFSET 1",
    )
    .expect("valid isNotNull projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::IsNotNull {
                name: "reading".to_owned(),
                alias: None,
            },
            SelectItem::IsNotNull {
                name: "reading".to_owned(),
                alias: Some("present".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "isNotNull(reading)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.order_by[1].name, "present");
    assert!(!select.order_by[1].descending);
    assert_eq!(select.limit, Some(2));
    assert_eq!(select.offset, Some(1));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT isNotNull(reading) FROM samples", limits)
        .expect("one isNotNull item fits the AST limit");
    assert_eq!(
        parse_with_limits("SELECT isNotNull(reading), reading FROM samples", limits,),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn projects_mixed_all_null_and_non_nullable_inputs() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (value Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL), (7), (NULL), (-2), (9); \
             CREATE TABLE all_null (value Nullable(Int64)); \
             INSERT INTO all_null VALUES (NULL), (NULL); \
             CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES \
             (1, 1.5, true, 'one'), (2, -0.5, false, '');",
        )
        .expect("setup");

    let mixed = query(
        &mut database,
        "SELECT value, isNotNull(value) AS present FROM readings \
         WHERE value IS NULL OR value >= 7 \
         ORDER BY present ASC LIMIT 3 OFFSET 1",
    );
    assert_eq!(
        mixed.columns,
        [
            ResultColumn {
                name: "value".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "present".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    );
    assert_eq!(
        mixed.rows,
        [
            vec![Value::Null(DataType::Int64), Value::Bool(false)],
            vec![Value::Int64(7), Value::Bool(true)],
            vec![Value::Int64(9), Value::Bool(true)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT isNotNull(value) FROM readings \
             ORDER BY isNotNull(value) DESC LIMIT 3",
        )
        .rows,
        [
            vec![Value::Bool(true)],
            vec![Value::Bool(true)],
            vec![Value::Bool(true)],
        ]
    );

    let all_null = query(
        &mut database,
        "SELECT isNotNull(value) FROM all_null ORDER BY isNotNull(value)",
    );
    assert_eq!(
        all_null.columns,
        [ResultColumn {
            name: "isNotNull(value)".to_owned(),
            data_type: DataType::Bool,
        }]
    );
    assert_eq!(
        all_null.rows,
        [vec![Value::Bool(false)], vec![Value::Bool(false)]]
    );

    let non_nullable = query(
        &mut database,
        "SELECT isNotNull(i), isNotNull(f), isNotNull(b), isNotNull(s) FROM samples",
    );
    assert_eq!(
        non_nullable.rows,
        [
            vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
            ],
            vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
            ],
        ]
    );
}

#[test]
fn projects_grouped_nullable_nullness_with_aggregates_having_order_and_pagination() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (value Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL), (2), (NULL), (1), (2), (3);",
        )
        .expect("setup");

    let grouped = query(
        &mut database,
        "SELECT isNotNull(value) AS present, value AS grouped_value, COUNT(*) AS rows \
         FROM readings GROUP BY value \
         ORDER BY present ASC, grouped_value ASC",
    );
    assert_eq!(
        grouped.columns,
        [
            ResultColumn {
                name: "present".to_owned(),
                data_type: DataType::Bool,
            },
            ResultColumn {
                name: "grouped_value".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "rows".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        grouped.rows,
        [
            vec![
                Value::Bool(false),
                Value::Null(DataType::Int64),
                Value::Int64(2),
            ],
            vec![Value::Bool(true), Value::Int64(1), Value::Int64(1)],
            vec![Value::Bool(true), Value::Int64(2), Value::Int64(2)],
            vec![Value::Bool(true), Value::Int64(3), Value::Int64(1)],
        ]
    );

    let paginated = query(
        &mut database,
        "SELECT COUNT(*) AS rows, isNotNull(value) AS present \
         FROM readings GROUP BY value HAVING rows >= 2 \
         ORDER BY isNotNull(value) ASC LIMIT 1 OFFSET 1",
    );
    assert_eq!(
        paginated.columns,
        [
            ResultColumn {
                name: "rows".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "present".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    );
    assert_eq!(paginated.rows, [vec![Value::Int64(2), Value::Bool(true)]]);
}

#[test]
fn projects_grouped_all_null_and_non_nullable_keys() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE all_null (value Nullable(Int64)); \
             INSERT INTO all_null VALUES (NULL), (NULL), (NULL); \
             CREATE TABLE labels (kind String); \
             INSERT INTO labels VALUES ('beta'), ('alpha'), ('beta');",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT isNotNull(value), COUNT(*) FROM all_null GROUP BY value",
        )
        .rows,
        [vec![Value::Bool(false), Value::Int64(3)]]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT kind, isNotNull(kind) AS present, COUNT(*) AS rows \
             FROM labels GROUP BY kind ORDER BY isNotNull(kind), kind",
        )
        .rows,
        [
            vec![
                Value::String("alpha".to_owned()),
                Value::Bool(true),
                Value::Int64(1),
            ],
            vec![
                Value::String("beta".to_owned()),
                Value::Bool(true),
                Value::Int64(2),
            ],
        ]
    );
}

#[test]
fn rejects_ungrouped_is_not_null_sources_expression_grouping_and_malformed_shapes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (value Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL), (1); \
             CREATE TABLE samples (value Int64, other Int64); \
             INSERT INTO samples VALUES (1, 10), (2, 20);",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT isNotNull(missing) FROM readings"),
        Err(Error::ColumnNotFound {
            table: "readings".to_owned(),
            column: "missing".to_owned(),
        })
    );

    for (sql, column) in [
        ("SELECT isNotNull(value), COUNT(*) FROM readings", "value"),
        (
            "SELECT isNotNull(other), COUNT(*) FROM samples GROUP BY value",
            "other",
        ),
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(format!(
                "column '{column}' must appear in GROUP BY"
            ))),
            "{sql}"
        );
    }

    for sql in [
        "SELECT isNotNull(value) FROM readings GROUP BY isNotNull(value)",
        "SELECT isNotNull() FROM readings",
        "SELECT isNotNull(*) FROM readings",
        "SELECT isNotNull(value, value) FROM readings",
        "SELECT isNotNull(value FROM readings",
        "SELECT isNotNull(value) present FROM readings",
        "SELECT isNotNull(isNotNull(value)) FROM readings",
        "SELECT isNotNull(value) FROM readings ORDER BY isNotNull()",
        "SELECT isNotNull(value) FROM readings ORDER BY isNotNull(*)",
        "SELECT isNotNull(value) FROM readings ORDER BY isNotNull(value, value)",
        "SELECT isNotNull(value) FROM readings ORDER BY isNotNull(value",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn obeys_selected_and_retained_result_bounds() {
    let setup = "CREATE TABLE readings (value Nullable(Int64)); \
                 INSERT INTO readings VALUES (1), (NULL), (2);";
    let result_name = "present";
    let exact_bytes = size_of::<ResultColumn>()
        + result_name.len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>();
    let sql = "SELECT isNotNull(value) AS present FROM readings LIMIT 1 OFFSET 1";

    let exact_limits = QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
    exact.execute(setup).expect("setup");
    assert_eq!(query(&mut exact, sql).rows, [vec![Value::Bool(false)]]);
    assert_eq!(
        exact.execute("SELECT isNotNull(value) FROM readings"),
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
        value_limited.execute("SELECT value, isNotNull(value) AS present FROM readings LIMIT 2",),
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

    let grouped_sql = "SELECT isNotNull(value) AS present FROM readings \
                       GROUP BY value ORDER BY present ASC LIMIT 1";
    assert_eq!(
        query(&mut exact, grouped_sql).rows,
        [vec![Value::Bool(false)]]
    );
    retained
        .execute_with_result_limit(grouped_sql, exact_bytes)
        .expect("exact grouped retained-result bound succeeds");
    assert_eq!(
        retained.execute_with_result_limit(grouped_sql, exact_bytes - 1),
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
        group_limited.execute(grouped_sql),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT groups",
            actual: 3,
            max: 2,
        })
    );
}

#[test]
fn emits_is_not_null_in_every_cli_output_format() {
    let sql = "CREATE TABLE samples (reading Nullable(Int64)); \
               INSERT INTO samples VALUES (NULL), (-2), (5); \
               SELECT isNotNull(reading) AS present FROM samples ORDER BY present;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+---------+\n\
         | present |\n\
         +---------+\n\
         | false   |\n\
         | true    |\n\
         | true    |\n\
         +---------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(
        String::from_utf8(csv).unwrap(),
        "present\nfalse\ntrue\ntrue\n"
    );

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(
        String::from_utf8(tsv).unwrap(),
        "present\nfalse\ntrue\ntrue\n"
    );

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"present\",\"type\":\"Bool\"}],\"rows\":[[false],[true],[true]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"present\":false}\n{\"present\":true}\n{\"present\":true}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[false]\n[true]\n[true]\n"
    );
}
