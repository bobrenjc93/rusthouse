use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::value::Value;

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().next().expect("one statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected a query result"),
    }
}

#[test]
fn parses_exact_drop_table_syntax_with_optional_semicolon_and_casing() {
    for (sql, expected_name) in [
        ("DROP TABLE events", "events"),
        ("drop table Events;", "Events"),
    ] {
        assert_eq!(
            parse(sql).expect("valid DROP TABLE"),
            [Statement::DropTable {
                name: expected_name.to_owned(),
            }]
        );
    }

    for (sql, expected_name) in [
        ("DROP TABLE IF EXISTS events", "events"),
        ("drop table if exists Events;", "Events"),
        ("DrOp TaBlE iF eXiStS EVENTS", "EVENTS"),
    ] {
        assert_eq!(
            parse(sql).expect("valid conditional DROP TABLE"),
            [Statement::DropTableIfExists {
                name: expected_name.to_owned(),
            }]
        );
    }

    assert_eq!(
        parse("DROP TABLE IF").expect("IF remains a legal table name"),
        [Statement::DropTable {
            name: "IF".to_owned(),
        }]
    );
}

#[test]
fn original_drop_table_ast_shape_remains_directly_executable() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64);")
        .expect("setup succeeds");

    assert_eq!(
        database
            .execute_statement(Statement::DropTable {
                name: "EVENTS".to_owned(),
            })
            .expect("the original public AST shape remains supported"),
        StatementResult::Command {
            tag: "DROP TABLE",
            affected_rows: 0,
        }
    );
}

#[test]
fn rejects_malformed_drop_table_syntax_with_typed_sql_errors() {
    let trailing = "DROP TABLE events CASCADE";
    assert_eq!(
        parse(trailing).expect_err("DROP TABLE has no trailing clauses"),
        Error::Sql {
            position: trailing.find("CASCADE").unwrap(),
            message: "unexpected trailing input after DROP TABLE".to_owned(),
        }
    );

    assert_eq!(
        parse("DROP events").expect_err("TABLE keyword is required"),
        Error::Sql {
            position: 5,
            message: "expected keyword TABLE".to_owned(),
        }
    );
    assert_eq!(
        parse("DROP TABLE").expect_err("table name is required"),
        Error::Sql {
            position: 10,
            message: "expected table name".to_owned(),
        }
    );

    let conditional_trailing = "DROP TABLE IF EXISTS events CASCADE";
    assert_eq!(
        parse(conditional_trailing).expect_err("conditional DROP has no trailing clauses"),
        Error::Sql {
            position: conditional_trailing.find("CASCADE").unwrap(),
            message: "unexpected trailing input after DROP TABLE".to_owned(),
        }
    );
    assert_eq!(
        parse("DROP TABLE IF EXISTS").expect_err("conditional table name is required"),
        Error::Sql {
            position: "DROP TABLE IF EXISTS".len(),
            message: "expected table name".to_owned(),
        }
    );
    let malformed_modifier = "DROP TABLE IF NOT EXISTS events";
    assert_eq!(
        parse(malformed_modifier).expect_err("the modifier must be exactly IF EXISTS"),
        Error::Sql {
            position: malformed_modifier.find("NOT").unwrap(),
            message: "unexpected trailing input after DROP TABLE".to_owned(),
        }
    );
}

#[test]
fn create_show_drop_select_lifecycle_preserves_unrelated_tables() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Events (id Int64); \
             INSERT INTO Events VALUES (7); \
             CREATE TABLE retained (id Int64); \
             INSERT INTO retained VALUES (11);",
        )
        .expect("setup succeeds");

    assert_eq!(
        query(&mut database, "SHOW TABLES;").rows,
        [
            vec![Value::String("Events".to_owned())],
            vec![Value::String("retained".to_owned())],
        ]
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM EVENTS;").rows,
        [vec![Value::Int64(7)]]
    );

    assert_eq!(
        database
            .execute("DROP TABLE IF EXISTS eVeNtS;")
            .expect("case-insensitive drop succeeds"),
        [StatementResult::Command {
            tag: "DROP TABLE",
            affected_rows: 0,
        }]
    );
    assert_eq!(
        query(&mut database, "SHOW TABLES;").rows,
        [vec![Value::String("retained".to_owned())]]
    );
    assert_eq!(
        database
            .execute("SELECT id FROM Events;")
            .expect_err("the dropped table is gone"),
        Error::TableNotFound("Events".to_owned())
    );

    assert_eq!(
        database
            .execute("dRoP tAbLe If ExIsTs EVENTS;")
            .expect("a missing conditional drop is a no-op"),
        [StatementResult::Command {
            tag: "DROP TABLE",
            affected_rows: 0,
        }]
    );
    assert_eq!(
        database
            .execute("DROP TABLE missing;")
            .expect_err("missing table uses the existing typed error"),
        Error::TableNotFound("missing".to_owned())
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM RETAINED;").rows,
        [vec![Value::Int64(11)]]
    );
    assert_eq!(
        query(&mut database, "SHOW TABLES;").rows,
        [vec![Value::String("retained".to_owned())]]
    );
}
