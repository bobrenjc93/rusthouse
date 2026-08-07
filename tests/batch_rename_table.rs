use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::value::Value;
use rusthouse::{SharedDatabase, SharedDatabaseError};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().next().expect("one statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected a query result"),
    }
}

#[test]
fn parses_exact_rename_table_syntax_with_optional_semicolon_and_casing() {
    for (sql, source, destination) in [
        ("RENAME TABLE events TO archived", "events", "archived"),
        (
            "rename table Events to EventArchive;",
            "Events",
            "EventArchive",
        ),
    ] {
        assert_eq!(
            parse(sql).expect("valid RENAME TABLE"),
            [Statement::RenameTable {
                source: source.to_owned(),
                destination: destination.to_owned(),
            }]
        );
    }
}

#[test]
fn rejects_malformed_rename_table_syntax_with_typed_sql_errors() {
    assert_eq!(
        parse("RENAME events TO archived").expect_err("TABLE keyword is required"),
        Error::Sql {
            position: 7,
            message: "expected keyword TABLE".to_owned(),
        }
    );
    assert_eq!(
        parse("RENAME TABLE events archived").expect_err("TO keyword is required"),
        Error::Sql {
            position: 20,
            message: "expected keyword TO".to_owned(),
        }
    );
    assert_eq!(
        parse("RENAME TABLE events TO").expect_err("destination is required"),
        Error::Sql {
            position: 22,
            message: "expected destination table name".to_owned(),
        }
    );

    let trailing = "RENAME TABLE events TO archived ON CLUSTER cluster";
    assert_eq!(
        parse(trailing).expect_err("RENAME TABLE has no trailing clauses"),
        Error::Sql {
            position: trailing.find("ON").unwrap(),
            message: "unexpected trailing input after RENAME TABLE".to_owned(),
        }
    );
}

#[test]
fn rename_preserves_schema_rows_row_cap_and_destination_display_case() {
    let mut database = Database::with_max_rows_per_table(2);
    database
        .execute(
            "CREATE TABLE EventLog (id Int64, label String); \
             INSERT INTO EventLog VALUES (2, 'second'), (1, 'first');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database
            .execute("RENAME TABLE eVeNtLoG TO HistoricalEvents;")
            .expect("case-insensitive source resolves"),
        [StatementResult::Command {
            tag: "RENAME TABLE",
            affected_rows: 0,
        }]
    );
    assert_eq!(
        database
            .catalog()
            .table("EventLog")
            .expect_err("the source key is removed"),
        Error::TableNotFound("EventLog".to_owned())
    );
    let renamed = database
        .catalog()
        .table("HISTORICALEVENTS")
        .expect("destination resolves case-insensitively");
    assert_eq!(renamed.name(), "HistoricalEvents");
    assert_eq!(renamed.row_count(), 2);
    assert_eq!(renamed.row_cap(), 2);
    assert_eq!(renamed.schema()[0].name, "id");
    assert_eq!(renamed.schema()[1].name, "label");

    assert_eq!(
        query(
            &mut database,
            "SELECT id, label FROM historicalevents ORDER BY id;"
        )
        .rows,
        [
            vec![Value::Int64(1), Value::String("first".to_owned())],
            vec![Value::Int64(2), Value::String("second".to_owned())],
        ]
    );
    assert_eq!(
        query(&mut database, "SHOW TABLES;").rows,
        [vec![Value::String("HistoricalEvents".to_owned())]]
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE HISTORICALEVENTS;").rows,
        [vec![Value::String(
            "CREATE TABLE HistoricalEvents (id Int64, label String)".to_owned()
        )]]
    );
    assert_eq!(
        database.execute("INSERT INTO HistoricalEvents VALUES (3, 'third');"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 2,
        })
    );
}

#[test]
fn case_only_rename_deterministically_replaces_the_display_name() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Events (id Int64); \
             INSERT INTO Events VALUES (7); \
             RENAME TABLE EVENTS TO eVENTS;",
        )
        .expect("case-only rename succeeds");

    assert_eq!(
        query(&mut database, "SHOW TABLES;").rows,
        [vec![Value::String("eVENTS".to_owned())]]
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE events;").rows,
        [vec![Value::String(
            "CREATE TABLE eVENTS (id Int64)".to_owned()
        )]]
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM EvEnTs;").rows,
        [vec![Value::Int64(7)]]
    );
}

#[test]
fn missing_source_and_destination_collision_leave_the_catalog_unchanged() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Source (id Int64); \
             INSERT INTO Source VALUES (1); \
             CREATE TABLE Destination (id Int64); \
             INSERT INTO Destination VALUES (2);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("RENAME TABLE missing TO destination;"),
        Err(Error::TableNotFound("missing".to_owned()))
    );
    assert_eq!(
        database.execute("RENAME TABLE source TO dEsTiNaTiOn;"),
        Err(Error::TableAlreadyExists("dEsTiNaTiOn".to_owned()))
    );

    assert_eq!(
        query(&mut database, "SHOW TABLES;").rows,
        [
            vec![Value::String("Destination".to_owned())],
            vec![Value::String("Source".to_owned())],
        ]
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM source;").rows,
        [vec![Value::Int64(1)]]
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM destination;").rows,
        [vec![Value::Int64(2)]]
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE SOURCE;").rows,
        [vec![Value::String(
            "CREATE TABLE Source (id Int64)".to_owned()
        )]]
    );
}

#[test]
fn shared_database_renames_under_the_write_lock_and_exposes_the_new_name() {
    let database = SharedDatabase::with_max_rows_per_table(1);
    let other_handle = database.clone();
    database
        .execute(
            "CREATE TABLE LiveEvents (id Int64); \
             INSERT INTO LiveEvents VALUES (9);",
        )
        .expect("setup succeeds");

    assert_eq!(
        other_handle
            .execute("RENAME TABLE liveevents TO ArchivedEvents;")
            .expect("shared mutation succeeds"),
        [StatementResult::Command {
            tag: "RENAME TABLE",
            affected_rows: 0,
        }]
    );
    assert_eq!(
        database
            .query("SELECT id FROM archiveDevents;")
            .expect("new name is visible across handles")
            .rows,
        [vec![Value::Int64(9)]]
    );
    assert_eq!(
        database.query("SELECT id FROM LiveEvents;"),
        Err(SharedDatabaseError::Sql(Error::TableNotFound(
            "LiveEvents".to_owned()
        )))
    );
    assert_eq!(
        database
            .query("SHOW CREATE TABLE archiveDevents;")
            .expect("metadata uses the destination display name")
            .rows,
        [vec![Value::String(
            "CREATE TABLE ArchivedEvents (id Int64)".to_owned()
        )]]
    );
    assert_eq!(
        database.query("RENAME TABLE ArchivedEvents TO current;"),
        Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "RENAME TABLE",
        })
    );
}
