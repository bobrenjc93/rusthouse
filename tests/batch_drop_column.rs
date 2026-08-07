use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::storage::{Column, ColumnDef};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{DatabaseMetrics, SharedDatabase, SharedDatabaseError};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    result.clone()
}

#[test]
fn parses_exact_alter_table_drop_column_syntax() {
    for (sql, table, column) in [
        (
            "ALTER TABLE events DROP COLUMN payload",
            "events",
            "payload",
        ),
        (
            "alter table Events drop column Payload;",
            "Events",
            "Payload",
        ),
    ] {
        assert_eq!(
            parse(sql).expect("valid ALTER TABLE DROP COLUMN"),
            [Statement::DropColumn {
                table: table.to_owned(),
                column: column.to_owned(),
            }]
        );
    }
}

#[test]
fn rejects_every_other_alter_table_drop_shape() {
    for sql in [
        "ALTER events DROP COLUMN payload",
        "ALTER TABLE events DROP payload",
        "ALTER TABLE events DROP COLUMN",
        "ALTER TABLE events DROP COLUMN IF EXISTS payload",
    ] {
        assert!(matches!(parse(sql), Err(Error::Sql { .. })), "{sql}");
    }

    let trailing = "ALTER TABLE events DROP COLUMN payload CASCADE";
    assert_eq!(
        parse(trailing),
        Err(Error::Sql {
            position: trailing.find("CASCADE").unwrap(),
            message: "unexpected trailing input after ALTER TABLE DROP COLUMN".to_owned(),
        })
    );
}

#[test]
fn drop_removes_the_aligned_typed_vector_and_preserves_table_state() {
    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute(
            "CREATE TABLE Metrics (id Int64, score Float64, active Bool, label String); \
             INSERT INTO metrics VALUES \
                 (2, 4.5, false, 'second'), \
                 (1, 2.5, true, 'first');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE mEtRiCs DROP COLUMN sCoRe;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        }])
    );

    let table = database.catalog().table("METRICS").expect("table remains");
    assert_eq!(table.name(), "Metrics");
    assert_eq!(table.row_count(), 2);
    assert_eq!(table.row_cap(), 3);
    assert_eq!(
        table.schema(),
        [
            ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ColumnDef {
                name: "active".to_owned(),
                data_type: DataType::Bool,
            },
            ColumnDef {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &[2, 1]));
    assert!(matches!(&table.columns()[1], Column::Bool(values) if values == &[false, true]));
    assert!(
        matches!(&table.columns()[2], Column::String(values) if values == &["second", "first"])
    );

    assert_eq!(
        query(&mut database, "SELECT id, active, label FROM metrics;").rows,
        [
            vec![
                Value::Int64(2),
                Value::Bool(false),
                Value::String("second".to_owned()),
            ],
            vec![
                Value::Int64(1),
                Value::Bool(true),
                Value::String("first".to_owned()),
            ],
        ]
    );
    assert_eq!(
        database.execute("SELECT score FROM metrics;"),
        Err(Error::ColumnNotFound {
            table: "Metrics".to_owned(),
            column: "score".to_owned(),
        })
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE metrics;").rows,
        [vec![Value::String(
            "CREATE TABLE Metrics (id Int64, active Bool, label String)".to_owned()
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
                Value::String("active".to_owned()),
                Value::String("Bool".to_owned()),
            ],
            vec![
                Value::String("label".to_owned()),
                Value::String("String".to_owned()),
            ],
        ]
    );

    database
        .execute("INSERT INTO metrics VALUES (3, true, 'third');")
        .expect("the remaining schema accepts a row");
    assert_eq!(
        database.execute("INSERT INTO metrics VALUES (4, false, 'over cap');"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 4,
            max: 3,
        })
    );
}

#[test]
fn missing_and_sole_column_failures_leave_tables_unchanged() {
    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute(
            "CREATE TABLE Events (id Int64, payload String); \
             INSERT INTO Events VALUES (7, 'kept'); \
             CREATE TABLE Solo (only_value Bool); \
             INSERT INTO Solo VALUES (true);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE missing DROP COLUMN payload;"),
        Err(Error::TableNotFound("missing".to_owned()))
    );
    assert_eq!(
        database.execute("ALTER TABLE events DROP COLUMN absent;"),
        Err(Error::ColumnNotFound {
            table: "Events".to_owned(),
            column: "absent".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE solo DROP COLUMN absent;"),
        Err(Error::ColumnNotFound {
            table: "Solo".to_owned(),
            column: "absent".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE SOLO DROP COLUMN ONLY_VALUE;"),
        Err(Error::InvalidQuery(
            "cannot drop the only column from table 'Solo'".to_owned()
        ))
    );

    let events = database.catalog().table("events").expect("events remains");
    assert_eq!(events.row_count(), 1);
    assert_eq!(events.row_cap(), 3);
    assert_eq!(
        events
            .schema()
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "payload"]
    );
    let solo = database.catalog().table("solo").expect("solo remains");
    assert_eq!(solo.row_count(), 1);
    assert_eq!(solo.row_cap(), 3);
    assert_eq!(solo.schema()[0].name, "only_value");

    assert_eq!(
        query(&mut database, "SELECT id, payload FROM events;").rows,
        [vec![Value::Int64(7), Value::String("kept".to_owned())]]
    );
    assert_eq!(
        query(&mut database, "SELECT only_value FROM solo;").rows,
        [vec![Value::Bool(true)]]
    );
}

#[test]
fn shared_database_serializes_drop_and_updates_metadata() {
    let database = SharedDatabase::with_max_rows_per_table(2);
    let other_handle = database.clone();
    database
        .execute(
            "CREATE TABLE Events (id Int64, payload String, active Bool); \
             INSERT INTO Events VALUES (1, 'first', true), (2, 'second', false);",
        )
        .expect("setup succeeds");

    assert_eq!(
        other_handle
            .execute("ALTER TABLE EVENTS DROP COLUMN PAYLOAD;")
            .expect("shared mutation succeeds"),
        [StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        }]
    );
    assert_eq!(
        database.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 1,
            column_count: 2,
            retained_row_count: 2,
        })
    );

    let selected = database
        .query("SELECT id, active FROM events;")
        .expect("drop is visible across handles");
    assert_eq!(
        selected.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "active".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    );
    assert_eq!(
        selected.rows,
        [
            vec![Value::Int64(1), Value::Bool(true)],
            vec![Value::Int64(2), Value::Bool(false)],
        ]
    );
    assert_eq!(
        database.query("ALTER TABLE events DROP COLUMN active;"),
        Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "ALTER TABLE",
        })
    );
}
