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
fn parses_exact_alter_table_add_column_syntax_for_every_type() {
    for (sql, table, name, data_type) in [
        (
            "ALTER TABLE events ADD COLUMN count Int64",
            "events",
            "count",
            DataType::Int64,
        ),
        (
            "alter table Events add column Ratio float64;",
            "Events",
            "Ratio",
            DataType::Float64,
        ),
        (
            "ALTER TABLE events ADD COLUMN active Bool",
            "events",
            "active",
            DataType::Bool,
        ),
        (
            "ALTER TABLE events ADD COLUMN label String;",
            "events",
            "label",
            DataType::String,
        ),
    ] {
        assert_eq!(
            parse(sql).expect("valid ALTER TABLE ADD COLUMN"),
            [Statement::AddColumn {
                table: table.to_owned(),
                column: ColumnDef {
                    name: name.to_owned(),
                    data_type,
                },
            }]
        );
    }
}

#[test]
fn rejects_every_other_alter_table_add_column_shape() {
    for sql in [
        "ALTER events ADD COLUMN payload String",
        "ALTER TABLE events ADD payload String",
        "ALTER TABLE events ADD COLUMN",
        "ALTER TABLE events ADD COLUMN payload",
        "ALTER TABLE events ADD COLUMN payload UInt64",
    ] {
        assert!(matches!(parse(sql), Err(Error::Sql { .. })), "{sql}");
    }

    let trailing = "ALTER TABLE events ADD COLUMN payload String DEFAULT 'x'";
    assert_eq!(
        parse(trailing),
        Err(Error::Sql {
            position: trailing.find("DEFAULT").unwrap(),
            message: "unexpected trailing input after ALTER TABLE ADD COLUMN".to_owned(),
        })
    );
}

#[test]
fn populated_table_backfills_every_physical_type_and_exposes_metadata() {
    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute(
            "CREATE TABLE Events (id Int64); \
             INSERT INTO events VALUES (2), (1);",
        )
        .expect("setup succeeds");

    for sql in [
        "ALTER TABLE EVENTS ADD COLUMN count Int64;",
        "ALTER TABLE events ADD COLUMN ratio Float64;",
        "ALTER TABLE Events ADD COLUMN active Bool;",
        "ALTER TABLE events ADD COLUMN label String;",
    ] {
        assert_eq!(
            database.execute(sql),
            Ok(vec![StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 0,
            }])
        );
    }

    let table = database.catalog().table("EVENTS").expect("table remains");
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
                name: "count".to_owned(),
                data_type: DataType::Int64,
            },
            ColumnDef {
                name: "ratio".to_owned(),
                data_type: DataType::Float64,
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
    assert!(matches!(&table.columns()[1], Column::Int64(values) if values == &[0, 0]));
    assert!(matches!(&table.columns()[2], Column::Float64(values) if values == &[0.0, 0.0]));
    assert!(matches!(&table.columns()[3], Column::Bool(values) if values == &[false, false]));
    assert!(matches!(&table.columns()[4], Column::String(values) if values == &["", ""]));

    let selected = query(
        &mut database,
        "SELECT id, count, ratio, active, label FROM events ORDER BY id;",
    );
    assert_eq!(
        selected.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "count".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "ratio".to_owned(),
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
                Value::Int64(0),
                Value::Float64(0.0),
                Value::Bool(false),
                Value::String(String::new()),
            ],
            vec![
                Value::Int64(2),
                Value::Int64(0),
                Value::Float64(0.0),
                Value::Bool(false),
                Value::String(String::new()),
            ],
        ]
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE events;").rows,
        [vec![Value::String(
            "CREATE TABLE Events (id Int64, count Int64, ratio Float64, active Bool, label String)"
                .to_owned()
        )]]
    );
    assert_eq!(
        query(&mut database, "DESCRIBE TABLE events;").rows,
        [
            vec![
                Value::String("id".to_owned()),
                Value::String("Int64".to_owned()),
            ],
            vec![
                Value::String("count".to_owned()),
                Value::String("Int64".to_owned()),
            ],
            vec![
                Value::String("ratio".to_owned()),
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

    database
        .execute("INSERT INTO events VALUES (3, 9, 2.5, true, 'new');")
        .expect("the extended schema accepts new typed rows");
    assert_eq!(
        query(
            &mut database,
            "SELECT count, ratio, active, label FROM events WHERE id = 3;",
        )
        .rows,
        [vec![
            Value::Int64(9),
            Value::Float64(2.5),
            Value::Bool(true),
            Value::String("new".to_owned()),
        ]]
    );
}

#[test]
fn every_failed_add_leaves_schema_physical_columns_rows_and_capacity_unchanged() {
    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute(
            "CREATE TABLE Events (id Int64, payload String); \
             INSERT INTO Events VALUES (7, 'kept');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE missing ADD COLUMN score Float64;"),
        Err(Error::TableNotFound("missing".to_owned()))
    );
    assert_eq!(
        database.execute("ALTER TABLE events ADD COLUMN ID Bool;"),
        Err(Error::DuplicateColumn("ID".to_owned()))
    );
    assert_eq!(
        database.execute("ALTER TABLE events ADD COLUMN TRUE Bool;"),
        Err(Error::ReservedIdentifier {
            identifier: "TRUE".to_owned(),
            context: "column name".to_owned(),
        })
    );
    assert_eq!(
        database.execute_statement(Statement::AddColumn {
            table: "events".to_owned(),
            column: ColumnDef {
                name: "bad name".to_owned(),
                data_type: DataType::String,
            },
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
        table.schema(),
        [
            ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ColumnDef {
                name: "payload".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &[7]));
    assert!(matches!(&table.columns()[1], Column::String(values) if values == &["kept"]));
}

#[test]
fn shared_database_serializes_add_updates_metrics_and_keeps_queries_read_only() {
    let database = SharedDatabase::with_max_rows_per_table(2);
    let other_handle = database.clone();
    database
        .execute(
            "CREATE TABLE Events (id Int64); \
             INSERT INTO Events VALUES (1), (2);",
        )
        .expect("setup succeeds");

    assert_eq!(
        other_handle
            .execute("ALTER TABLE EVENTS ADD COLUMN Label String;")
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
    assert_eq!(
        database
            .query("SELECT id, label FROM events ORDER BY id;")
            .expect("added column is visible across handles")
            .rows,
        [
            vec![Value::Int64(1), Value::String(String::new())],
            vec![Value::Int64(2), Value::String(String::new())],
        ]
    );
    assert_eq!(
        database.query("ALTER TABLE events ADD COLUMN active Bool;"),
        Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "ALTER TABLE",
        })
    );
}
