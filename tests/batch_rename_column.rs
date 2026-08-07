use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    result.clone()
}

#[test]
fn parses_exact_alter_table_rename_column_syntax() {
    for (sql, table, source, destination) in [
        (
            "ALTER TABLE events RENAME COLUMN payload TO message",
            "events",
            "payload",
            "message",
        ),
        (
            "alter table Events rename column Payload to Message;",
            "Events",
            "Payload",
            "Message",
        ),
    ] {
        assert_eq!(
            parse(sql).expect("valid ALTER TABLE RENAME COLUMN"),
            [Statement::RenameColumn {
                table: table.to_owned(),
                source: source.to_owned(),
                destination: destination.to_owned(),
            }]
        );
    }
}

#[test]
fn rejects_every_other_alter_table_rename_shape() {
    assert!(matches!(
        parse("ALTER events RENAME COLUMN payload TO message"),
        Err(Error::Sql { .. })
    ));
    assert!(matches!(
        parse("ALTER TABLE events RENAME payload TO message"),
        Err(Error::Sql { .. })
    ));
    assert!(matches!(
        parse("ALTER TABLE events RENAME COLUMN payload message"),
        Err(Error::Sql { .. })
    ));
    assert!(matches!(
        parse("ALTER TABLE events RENAME COLUMN payload TO"),
        Err(Error::Sql { .. })
    ));

    let trailing = "ALTER TABLE events RENAME COLUMN payload TO message AFTER id";
    assert_eq!(
        parse(trailing),
        Err(Error::Sql {
            position: trailing.find("AFTER").unwrap(),
            message: "unexpected trailing input after ALTER TABLE RENAME COLUMN".to_owned(),
        })
    );
}

#[test]
fn rename_changes_only_the_display_name_and_supports_case_only_changes() {
    let mut database = Database::with_max_rows_per_table(2);
    database
        .execute(
            "CREATE TABLE Metrics (id Int64, Score Float64, active Bool, label String); \
             INSERT INTO metrics VALUES \
                 (2, 4.5, false, 'second'), \
                 (1, 2.5, true, 'first');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE mEtRiCs RENAME COLUMN sCoRe TO Rating;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        }])
    );

    let table = database.catalog().table("METRICS").expect("table remains");
    assert_eq!(table.name(), "Metrics");
    assert_eq!(table.row_count(), 2);
    assert_eq!(table.row_cap(), 2);
    assert_eq!(
        table.schema(),
        [
            rusthouse::batch::storage::ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            rusthouse::batch::storage::ColumnDef {
                name: "Rating".to_owned(),
                data_type: DataType::Float64,
            },
            rusthouse::batch::storage::ColumnDef {
                name: "active".to_owned(),
                data_type: DataType::Bool,
            },
            rusthouse::batch::storage::ColumnDef {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
        ]
    );

    let selected = query(
        &mut database,
        "SELECT id, rating, active, label FROM metrics ORDER BY id;",
    );
    assert_eq!(
        selected.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "Rating".to_owned(),
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
        ]
    );
    assert_eq!(
        selected.rows,
        [
            vec![
                Value::Int64(1),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("first".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(4.5),
                Value::Bool(false),
                Value::String("second".to_owned()),
            ],
        ]
    );
    assert_eq!(
        database.execute("SELECT Score FROM metrics;"),
        Err(Error::ColumnNotFound {
            table: "Metrics".to_owned(),
            column: "Score".to_owned(),
        })
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE metrics;").rows,
        [vec![Value::String(
            "CREATE TABLE Metrics (id Int64, Rating Float64, active Bool, label String)".to_owned()
        )]]
    );
    assert_eq!(
        query(&mut database, "DESCRIBE TABLE metrics;").rows,
        [
            vec![
                Value::String("id".to_owned()),
                Value::String("Int64".to_owned()),
            ],
            vec![
                Value::String("Rating".to_owned()),
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
        database.execute("INSERT INTO metrics VALUES (3, 6.0, true, 'third');"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 2,
        })
    );

    database
        .execute("ALTER TABLE metrics RENAME COLUMN RATING TO rATING;")
        .expect("case-only rename succeeds");
    assert_eq!(
        query(&mut database, "SELECT rating FROM metrics;").columns[0].name,
        "rATING"
    );
}

#[test]
fn every_failed_rename_leaves_schema_rows_and_capacity_unchanged() {
    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute(
            "CREATE TABLE Events (id Int64, payload String); \
             INSERT INTO Events VALUES (7, 'kept');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE missing RENAME COLUMN payload TO message;"),
        Err(Error::TableNotFound("missing".to_owned()))
    );
    assert_eq!(
        database.execute("ALTER TABLE events RENAME COLUMN absent TO message;"),
        Err(Error::ColumnNotFound {
            table: "Events".to_owned(),
            column: "absent".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events RENAME COLUMN payload TO TRUE;"),
        Err(Error::ReservedIdentifier {
            identifier: "TRUE".to_owned(),
            context: "column name".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events RENAME COLUMN payload TO ID;"),
        Err(Error::DuplicateColumn("ID".to_owned()))
    );
    assert_eq!(
        database.execute_statement(Statement::RenameColumn {
            table: "events".to_owned(),
            source: "payload".to_owned(),
            destination: "bad name".to_owned(),
        }),
        Err(Error::InvalidIdentifier {
            identifier: "bad name".to_owned(),
            context: "column name".to_owned(),
        })
    );

    let table = database.catalog().table("events").expect("table remains");
    assert_eq!(table.row_count(), 1);
    assert_eq!(table.row_cap(), 3);
    assert_eq!(
        table
            .schema()
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "payload"]
    );
    assert_eq!(
        query(&mut database, "SELECT id, payload FROM events;").rows,
        [vec![Value::Int64(7), Value::String("kept".to_owned())]]
    );
}

#[test]
fn shared_database_serializes_rename_and_rejects_it_as_a_read_only_query() {
    let database = SharedDatabase::with_max_rows_per_table(1);
    let other_handle = database.clone();
    database
        .execute(
            "CREATE TABLE Events (id Int64, payload String); \
             INSERT INTO Events VALUES (9, 'shared');",
        )
        .expect("setup succeeds");

    assert_eq!(
        other_handle
            .execute("ALTER TABLE EVENTS RENAME COLUMN PAYLOAD TO Message;")
            .expect("shared mutation succeeds"),
        [StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        }]
    );
    assert_eq!(
        database
            .query("SELECT message FROM events;")
            .expect("renamed column is visible across handles")
            .rows,
        [vec![Value::String("shared".to_owned())]]
    );
    assert_eq!(
        database.query("ALTER TABLE events RENAME COLUMN message TO payload;"),
        Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "ALTER TABLE",
        })
    );
}
