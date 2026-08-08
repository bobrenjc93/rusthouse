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
fn parses_upper_as_a_bounded_select_item_with_an_optional_alias() {
    let statements = parse(
        "SELECT UPPER(label), upper(label) AS normalized FROM samples \
         WHERE label != 'skip' ORDER BY upper(label) DESC LIMIT 2 OFFSET 1",
    )
    .expect("valid UPPER projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Upper {
                name: "label".to_owned(),
                alias: None,
            },
            SelectItem::Upper {
                name: "label".to_owned(),
                alias: Some("normalized".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "UPPER(label)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));
    assert_eq!(select.offset, Some(1));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT UPPER(label) FROM samples", limits)
        .expect("one UPPER item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT UPPER(label), label FROM samples", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn projects_ascii_uppercase_through_filters_expression_alias_ordering_and_offset() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (label String, keep Bool); \
             INSERT INTO samples VALUES \
             ('', true), ('MiXeD', true), ('alpha', true), ('éClAiR', true), \
             ('東京abc', true), ('skip', false), ('beta', true);",
        )
        .expect("setup");

    let expression_ordered = query(
        &mut database,
        "SELECT UPPER(label) FROM samples \
         WHERE keep = true ORDER BY UPPER(label) LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        expression_ordered.columns,
        [ResultColumn {
            name: "UPPER(label)".to_owned(),
            data_type: DataType::String,
        }]
    );
    assert_eq!(
        expression_ordered.rows,
        [
            vec![Value::String("ALPHA".to_owned())],
            vec![Value::String("BETA".to_owned())],
        ]
    );

    let alias_ordered = query(
        &mut database,
        "SELECT UPPER(label) AS normalized FROM samples \
         WHERE keep = true ORDER BY normalized DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        alias_ordered.rows,
        [
            vec![Value::String("éCLAIR".to_owned())],
            vec![Value::String("MIXED".to_owned())],
        ]
    );
}

#[test]
fn rejects_unknown_non_string_and_grouped_upper_inputs_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT UPPER(missing) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );

    for (name, actual) in [
        ("i", DataType::Int64),
        ("f", DataType::Float64),
        ("b", DataType::Bool),
    ] {
        assert_eq!(
            database.execute(&format!("SELECT UPPER({name}) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("UPPER argument '{name}'"),
                expected: "String".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    for sql in [
        "SELECT UPPER(s) FROM samples GROUP BY s",
        "SELECT UPPER(s), COUNT(*) FROM samples",
        "SELECT UPPER(s), COUNT(*) FROM samples GROUP BY s",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(
                "UPPER projections are only supported in ungrouped SELECT queries".to_owned()
            )),
            "{sql}"
        );
    }
}

#[test]
fn rejects_malformed_upper_syntax() {
    for sql in [
        "SELECT UPPER() FROM samples",
        "SELECT UPPER(*) FROM samples",
        "SELECT UPPER('text') FROM samples",
        "SELECT UPPER(label, label) FROM samples",
        "SELECT UPPER(label FROM samples",
        "SELECT UPPER(label) normalized FROM samples",
        "SELECT UPPER(UPPER(label)) FROM samples",
        "SELECT UPPER(label) FROM samples ORDER BY UPPER()",
        "SELECT UPPER(label) FROM samples ORDER BY UPPER(*)",
        "SELECT UPPER(label) FROM samples ORDER BY UPPER(label",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn upper_projection_obeys_exact_row_value_and_string_byte_result_caps() {
    let setup = "CREATE TABLE samples (label String); \
                 INSERT INTO samples VALUES ('one'), ('two'), ('three');";

    let mut row_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    row_limited.execute(setup).expect("setup");
    assert_eq!(
        query(&mut row_limited, "SELECT UPPER(label) FROM samples LIMIT 2").rows,
        [
            vec![Value::String("ONE".to_owned())],
            vec![Value::String("TWO".to_owned())],
        ]
    );
    assert_eq!(
        row_limited.execute("SELECT UPPER(label) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 3,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    value_limited.execute(setup).expect("setup");
    assert_eq!(
        value_limited.execute("SELECT UPPER(label) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 3,
            max: 2,
        })
    );

    let result_name = "normalized";
    let fixed_bytes = std::mem::size_of::<ResultColumn>()
        + result_name.len()
        + std::mem::size_of::<Vec<Value>>()
        + std::mem::size_of::<Value>();
    let input = "éMiXeD";
    let exact_bytes = fixed_bytes + input.len();
    let setup = "CREATE TABLE samples (label String); INSERT INTO samples VALUES ('éMiXeD');";
    let sql = "SELECT UPPER(label) AS normalized FROM samples";

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(setup).expect("setup");
    assert_eq!(
        query(&mut exact, sql).rows,
        [vec![Value::String("éMIXED".to_owned())]]
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes - 1,
        ..QueryResultLimits::default()
    });
    byte_limited.execute(setup).expect("setup");
    assert_eq!(
        byte_limited.execute(sql),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            actual: exact_bytes,
            max: exact_bytes - 1,
        })
    );
}

#[test]
fn emits_upper_in_all_batch_output_formats() {
    let sql = "CREATE TABLE samples (label String); \
               INSERT INTO samples VALUES ('MiXeD'), ('éClAiR'), ('東京abc'); \
               SELECT UPPER(label) AS normalized FROM samples ORDER BY normalized;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+------------+\n\
         | normalized |\n\
         +------------+\n\
         | MIXED      |\n\
         | éCLAIR     |\n\
         | 東京ABC      |\n\
         +------------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(
        String::from_utf8(csv).unwrap(),
        "normalized\nMIXED\néCLAIR\n東京ABC\n"
    );

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(
        String::from_utf8(tsv).unwrap(),
        "normalized\nMIXED\néCLAIR\n東京ABC\n"
    );

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"normalized\",\"type\":\"String\"}],\"rows\":[[\"MIXED\"],[\"éCLAIR\"],[\"東京ABC\"]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"normalized\":\"MIXED\"}\n{\"normalized\":\"éCLAIR\"}\n{\"normalized\":\"東京ABC\"}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[\"MIXED\"]\n[\"éCLAIR\"]\n[\"東京ABC\"]\n"
    );
}
