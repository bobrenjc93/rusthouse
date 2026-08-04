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
fn parses_length_as_a_bounded_select_item_with_an_optional_alias() {
    let statements = parse(
        "SELECT LENGTH(label), length(label) AS bytes FROM samples \
         WHERE label != 'skip' ORDER BY bytes DESC LIMIT 2",
    )
    .expect("valid LENGTH projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Length {
                name: "label".to_owned(),
                alias: None,
            },
            SelectItem::Length {
                name: "label".to_owned(),
                alias: Some("bytes".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "bytes");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT LENGTH(label) FROM samples", limits)
        .expect("one LENGTH item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT LENGTH(label), label FROM samples", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn projects_utf8_byte_lengths_through_where_ordering_and_limit() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (label String, keep Bool); \
             INSERT INTO samples VALUES \
             ('', true), ('é', true), ('東京', true), ('discard', false);",
        )
        .expect("setup");

    let ordered = query(
        &mut database,
        "SELECT LENGTH(label) AS bytes FROM samples \
         WHERE keep = true ORDER BY bytes DESC LIMIT 2",
    );
    assert_eq!(
        ordered.columns,
        [ResultColumn {
            name: "bytes".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(ordered.rows, [vec![Value::Int64(6)], vec![Value::Int64(2)]]);

    let empty = query(
        &mut database,
        "SELECT LENGTH(label) FROM samples WHERE label = ''",
    );
    assert_eq!(
        empty.columns,
        [ResultColumn {
            name: "LENGTH(label)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(empty.rows, [vec![Value::Int64(0)]]);
}

#[test]
fn rejects_unknown_non_string_and_grouped_length_inputs_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT LENGTH(missing) FROM samples"),
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
            database.execute(&format!("SELECT LENGTH({name}) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("LENGTH argument '{name}'"),
                expected: "String".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    assert_eq!(
        database.execute("SELECT LENGTH(s), COUNT(*) FROM samples GROUP BY s"),
        Err(Error::InvalidQuery(
            "LENGTH projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
}

#[test]
fn rejects_malformed_length_syntax() {
    for sql in [
        "SELECT LENGTH() FROM samples",
        "SELECT LENGTH(*) FROM samples",
        "SELECT LENGTH('text') FROM samples",
        "SELECT LENGTH(label, label) FROM samples",
        "SELECT LENGTH(label FROM samples",
        "SELECT LENGTH(label) bytes FROM samples",
        "SELECT LENGTH(LENGTH(label)) FROM samples",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn emits_length_as_int64_in_csv_and_json() {
    let sql = "CREATE TABLE samples (label String); \
               INSERT INTO samples VALUES (''), ('é'), ('東京'); \
               SELECT LENGTH(label) AS bytes FROM samples ORDER BY bytes;";

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "bytes\n0\n2\n6\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"bytes\",\"type\":\"Int64\"}],\"rows\":[[0],[2],[6]]}\n"
    );
}
