use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().next().expect("one statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected a query result"),
    }
}

#[test]
fn parses_exact_truncate_table_syntax_with_optional_semicolon_and_casing() {
    for (sql, expected_name) in [
        ("TRUNCATE TABLE events", "events"),
        ("truncate table Events;", "Events"),
    ] {
        assert_eq!(
            parse(sql).expect("valid TRUNCATE TABLE"),
            [Statement::TruncateTable {
                name: expected_name.to_owned(),
            }]
        );
    }
}

#[test]
fn rejects_malformed_truncate_table_syntax_with_typed_sql_errors() {
    let trailing = "TRUNCATE TABLE events RESTART IDENTITY";
    assert_eq!(
        parse(trailing).expect_err("TRUNCATE TABLE has no trailing clauses"),
        Error::Sql {
            position: trailing.find("RESTART").unwrap(),
            message: "unexpected trailing input after TRUNCATE TABLE".to_owned(),
        }
    );

    assert_eq!(
        parse("TRUNCATE events").expect_err("TABLE keyword is required"),
        Error::Sql {
            position: 9,
            message: "expected keyword TABLE".to_owned(),
        }
    );
    assert_eq!(
        parse("TRUNCATE TABLE").expect_err("table name is required"),
        Error::Sql {
            position: 14,
            message: "expected table name".to_owned(),
        }
    );
}

#[test]
fn truncate_clears_all_physical_types_and_preserves_the_table_lifecycle() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Metrics (
                 id Int64, score Float64, active Bool, label String
             );
             INSERT INTO Metrics VALUES
                 (1, 1.5, true, 'first'),
                 (2, 2.5, false, 'second');
             CREATE TABLE retained (id Int64);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database
            .execute("TRUNCATE TABLE mEtRiCs;")
            .expect("case-insensitive truncate succeeds"),
        [StatementResult::Command {
            tag: "TRUNCATE TABLE",
            affected_rows: 2,
        }]
    );

    let table = database.catalog().table("METRICS").expect("table remains");
    assert_eq!(table.row_count(), 0);
    assert!(matches!(&table.columns()[0], Column::Int64(values) if values.is_empty()));
    assert!(matches!(&table.columns()[1], Column::Float64(values) if values.is_empty()));
    assert!(matches!(&table.columns()[2], Column::Bool(values) if values.is_empty()));
    assert!(matches!(&table.columns()[3], Column::String(values) if values.is_empty()));

    assert_eq!(
        query(&mut database, "SHOW TABLES;").rows,
        [
            vec![Value::String("Metrics".to_owned())],
            vec![Value::String("retained".to_owned())],
        ]
    );
    assert_eq!(
        query(&mut database, "DESCRIBE TABLE metrics;").rows,
        [
            vec![
                Value::String("id".to_owned()),
                Value::String("Int64".to_owned()),
            ],
            vec![
                Value::String("score".to_owned()),
                Value::String("Float64".to_owned()),
            ],
            vec![
                Value::String("active".to_owned()),
                Value::String("Bool".to_owned()),
            ],
            vec![
                Value::String("label".to_owned()),
                Value::String("String".to_owned()),
            ],
        ]
    );
    assert_eq!(
        query(&mut database, "SELECT * FROM metrics;"),
        QueryResult {
            columns: vec![
                ResultColumn {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "score".to_owned(),
                    data_type: DataType::Float64,
                },
                ResultColumn {
                    name: "active".to_owned(),
                    data_type: DataType::Bool,
                },
                ResultColumn {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![],
        }
    );

    database
        .execute("INSERT INTO metrics VALUES (3, 3.5, true, 'again');")
        .expect("the retained schema accepts reinsertion");
    assert_eq!(
        query(
            &mut database,
            "SELECT id, score, active, label FROM METRICS;"
        )
        .rows,
        [vec![
            Value::Int64(3),
            Value::Float64(3.5),
            Value::Bool(true),
            Value::String("again".to_owned()),
        ]]
    );

    assert_eq!(
        database
            .execute("TRUNCATE TABLE missing;")
            .expect_err("a missing table uses the typed catalog error"),
        Error::TableNotFound("missing".to_owned())
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM metrics;").rows,
        [vec![Value::Int64(3)]]
    );
}
