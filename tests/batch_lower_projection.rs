use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
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
fn parses_lower_as_a_bounded_select_item_with_an_optional_alias() {
    let statements = parse(
        "SELECT LOWER(label), lower(label) AS normalized FROM samples \
         WHERE label != 'skip' ORDER BY lower(label) DESC LIMIT 2",
    )
    .expect("valid LOWER projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Lower {
                name: "label".to_owned(),
                alias: None,
            },
            SelectItem::Lower {
                name: "label".to_owned(),
                alias: Some("normalized".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "LOWER(label)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT LOWER(label) FROM samples", limits)
        .expect("one LOWER item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT LOWER(label), label FROM samples", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn projects_ascii_lowercase_through_filters_expression_and_alias_ordering_and_limits() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (label String, keep Bool); \
             INSERT INTO samples VALUES \
             ('', true), ('MiXeD', true), ('ALPHA', true), ('ÉCLAIR', true), \
             ('東京ABC', true), ('SKIP', false), ('beta', true);",
        )
        .expect("setup");

    let expression_ordered = query(
        &mut database,
        "SELECT LOWER(label) FROM samples \
         WHERE keep = true ORDER BY LOWER(label) LIMIT 3",
    );
    assert_eq!(
        expression_ordered.columns,
        [ResultColumn {
            name: "LOWER(label)".to_owned(),
            data_type: DataType::String,
        }]
    );
    assert_eq!(
        expression_ordered.rows,
        [
            vec![Value::String(String::new())],
            vec![Value::String("alpha".to_owned())],
            vec![Value::String("beta".to_owned())],
        ]
    );

    let alias_ordered = query(
        &mut database,
        "SELECT LOWER(label) AS normalized FROM samples \
         WHERE keep = true ORDER BY normalized DESC LIMIT 3",
    );
    assert_eq!(
        alias_ordered.rows,
        [
            vec![Value::String("東京abc".to_owned())],
            vec![Value::String("Éclair".to_owned())],
            vec![Value::String("mixed".to_owned())],
        ]
    );
}

#[test]
fn rejects_unknown_non_string_and_grouped_lower_inputs_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'ONE');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT LOWER(missing) FROM samples"),
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
            database.execute(&format!("SELECT LOWER({name}) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("LOWER argument '{name}'"),
                expected: "String".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    for sql in [
        "SELECT LOWER(s) FROM samples GROUP BY s",
        "SELECT LOWER(s), COUNT(*) FROM samples",
        "SELECT LOWER(s), COUNT(*) FROM samples GROUP BY s",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(
                "LOWER projections are only supported in ungrouped SELECT queries".to_owned()
            )),
            "{sql}"
        );
    }
}

#[test]
fn rejects_malformed_lower_syntax() {
    for sql in [
        "SELECT LOWER() FROM samples",
        "SELECT LOWER(*) FROM samples",
        "SELECT LOWER('TEXT') FROM samples",
        "SELECT LOWER(label, label) FROM samples",
        "SELECT LOWER(label FROM samples",
        "SELECT LOWER(label) normalized FROM samples",
        "SELECT LOWER(LOWER(label)) FROM samples",
        "SELECT LOWER(label) FROM samples ORDER BY LOWER()",
        "SELECT LOWER(label) FROM samples ORDER BY LOWER(*)",
        "SELECT LOWER(label) FROM samples ORDER BY LOWER(label",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn lower_projection_obeys_row_value_and_string_byte_result_caps() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (label String); \
             INSERT INTO samples VALUES ('ONE'), ('TWO'), ('THREE');",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT LOWER(label) FROM samples LIMIT 2").rows,
        [
            vec![Value::String("one".to_owned())],
            vec![Value::String("two".to_owned())],
        ]
    );
    assert_eq!(
        database.execute("SELECT LOWER(label) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );

    let result_name = "normalized";
    let fixed_bytes = std::mem::size_of::<ResultColumn>()
        + result_name.len()
        + std::mem::size_of::<Vec<Value>>()
        + std::mem::size_of::<Value>();
    let exact_bytes = fixed_bytes + "MiXeD".len();
    let setup = "CREATE TABLE samples (label String); INSERT INTO samples VALUES ('MiXeD');";
    let sql = "SELECT LOWER(label) AS normalized FROM samples";

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(setup).expect("setup");
    assert_eq!(
        query(&mut exact, sql).rows,
        [vec![Value::String("mixed".to_owned())]]
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
fn emits_lower_as_string_in_csv_and_json() {
    let sql = "CREATE TABLE samples (label String); \
               INSERT INTO samples VALUES ('MiXeD'), ('ÉCLAIR'), ('東京ABC'); \
               SELECT LOWER(label) AS normalized FROM samples ORDER BY normalized;";

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(
        String::from_utf8(csv).unwrap(),
        "normalized\nmixed\nÉclair\n東京abc\n"
    );

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"normalized\",\"type\":\"String\"}],\"rows\":[[\"mixed\"],[\"Éclair\"],[\"東京abc\"]]}\n"
    );
}
