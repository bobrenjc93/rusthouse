use std::fmt::Write as _;

use rusthouse::batch::engine::{
    DEFAULT_MAX_CELLS_PER_TABLE, DEFAULT_MAX_COLUMNS_PER_TABLE, Database, QueryResult,
    ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::storage::{Column, ColumnDef};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{DatabaseMetrics, SharedDatabase, SharedDatabaseError, TableLimits};

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
fn parses_exact_case_insensitive_nullable_int64_add_column_syntax() {
    for (sql, table, column) in [
        (
            "ALTER TABLE events ADD COLUMN measurement Nullable(Int64)",
            "events",
            "measurement",
        ),
        (
            "alter table Events add column Measurement nullable(int64);",
            "Events",
            "Measurement",
        ),
    ] {
        assert_eq!(
            parse(sql).expect("valid nullable ALTER TABLE ADD COLUMN"),
            [Statement::AddNullableInt64Column {
                table: table.to_owned(),
                column: column.to_owned(),
            }]
        );
    }
}

#[test]
fn parses_exact_case_insensitive_conditional_nullable_int64_add_column_syntax() {
    for (sql, table, column) in [
        (
            "ALTER TABLE events ADD COLUMN IF NOT EXISTS measurement Nullable(Int64)",
            "events",
            "measurement",
        ),
        (
            "alter table Events add column if not exists Measurement nullable(int64);",
            "Events",
            "Measurement",
        ),
    ] {
        assert_eq!(
            parse(sql).expect("valid conditional nullable ALTER TABLE ADD COLUMN"),
            [Statement::AddNullableInt64ColumnIfNotExists {
                table: table.to_owned(),
                column: column.to_owned(),
            }]
        );
    }
}

#[test]
fn rejects_malformed_and_unsupported_nullable_add_column_types() {
    for sql in [
        "ALTER TABLE events ADD COLUMN value Nullable",
        "ALTER TABLE events ADD COLUMN value Nullable()",
        "ALTER TABLE events ADD COLUMN value Nullable(String)",
        "ALTER TABLE events ADD COLUMN value Nullable(Nullable(Int64))",
        "ALTER TABLE events ADD COLUMN value Nullable(Int64",
        "ALTER TABLE events ADD COLUMN value Nullable(Int64))",
        "ALTER TABLE events ADD COLUMN value Nullable(Int64, String)",
        "ALTER TABLE events ADD COLUMN value Nullable(Int64) NULL",
        "ALTER TABLE events ADD COLUMN value NullableInt64",
    ] {
        assert!(matches!(parse(sql), Err(Error::Sql { .. })), "{sql}");
    }

    let unsupported = "ALTER TABLE events ADD COLUMN value Nullable(Float64)";
    assert_eq!(
        parse(unsupported),
        Err(Error::Sql {
            position: unsupported.find("Float64").unwrap(),
            message: "unsupported nullable type 'Float64'; expected Int64".to_owned(),
        })
    );
}

#[test]
fn rejects_every_other_alter_table_add_column_shape() {
    for sql in [
        "ALTER events ADD COLUMN payload String",
        "ALTER TABLE events ADD payload String",
        "ALTER TABLE events ADD COLUMN",
        "ALTER TABLE events ADD COLUMN payload",
        "ALTER TABLE events ADD COLUMN payload UInt64",
        "ALTER TABLE events ADD COLUMN IF EXISTS payload Nullable(Int64)",
        "ALTER TABLE events ADD COLUMN IF NOT EXISTS payload Int64",
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
fn default_table_limits_bound_repeated_add_column_amplification() {
    let mut database = Database::new();
    assert_eq!(
        database.table_limits(),
        TableLimits::new(
            rusthouse::batch::engine::DEFAULT_MAX_ROWS_PER_TABLE,
            DEFAULT_MAX_COLUMNS_PER_TABLE,
            DEFAULT_MAX_CELLS_PER_TABLE,
        )
    );
    assert_ne!(database.table_limits().max_columns, usize::MAX);
    assert_ne!(database.table_limits().max_cells, usize::MAX);

    let mut batch = String::from("CREATE TABLE wide (c0 Int64);");
    for index in 1..DEFAULT_MAX_COLUMNS_PER_TABLE {
        write!(batch, "ALTER TABLE wide ADD COLUMN c{index} Int64;").unwrap();
    }
    database
        .execute(&batch)
        .expect("the exact default column cap fits in one CLI-style batch");
    assert_eq!(
        database.catalog().table("wide").unwrap().schema().len(),
        DEFAULT_MAX_COLUMNS_PER_TABLE
    );

    assert_eq!(
        database.execute("ALTER TABLE wide ADD COLUMN overflow Int64;"),
        Err(Error::ResourceLimitExceeded {
            resource: "table columns",
            actual: DEFAULT_MAX_COLUMNS_PER_TABLE + 1,
            max: DEFAULT_MAX_COLUMNS_PER_TABLE,
        })
    );
    let table = database.catalog().table("wide").unwrap();
    assert_eq!(table.schema().len(), DEFAULT_MAX_COLUMNS_PER_TABLE);
    assert_eq!(table.columns().len(), DEFAULT_MAX_COLUMNS_PER_TABLE);
}

#[test]
fn add_column_accepts_the_exact_cell_limit_and_rejects_before_mutation() {
    let limits = TableLimits::new(4, 3, 6);
    let mut database = Database::with_table_limits(limits);
    database
        .execute(
            "CREATE TABLE events (id Int64); \
             INSERT INTO events VALUES (1), (2), (3); \
             ALTER TABLE events ADD COLUMN label String;",
        )
        .expect("three rows by two columns exactly fits six cells");

    let table = database.catalog().table("events").expect("table exists");
    assert_eq!(table.limits(), limits);
    assert_eq!(table.retained_cell_count(), 6);
    assert!(matches!(&table.columns()[1], Column::String(values) if values == &["", "", ""]));

    assert_eq!(
        database.execute("ALTER TABLE events ADD COLUMN active Bool;"),
        Err(Error::ResourceLimitExceeded {
            resource: "table cells",
            actual: 9,
            max: 6,
        })
    );
    let table = database.catalog().table("events").expect("table remains");
    assert_eq!(table.schema().len(), 2);
    assert_eq!(table.columns().len(), 2);
    assert_eq!(table.retained_cell_count(), 6);
    assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &[1, 2, 3]));
    assert!(matches!(&table.columns()[1], Column::String(values) if values == &["", "", ""]));

    database
        .execute(
            "ALTER TABLE events DROP COLUMN label; \
             ALTER TABLE events ADD COLUMN active Bool;",
        )
        .expect("dropping a column restores cell capacity");
    assert_eq!(
        database.execute("INSERT INTO events VALUES (4, true);"),
        Err(Error::ResourceLimitExceeded {
            resource: "table cells",
            actual: 8,
            max: 6,
        })
    );
    let table = database.catalog().table("events").expect("table remains");
    assert_eq!(table.row_count(), 3);
    assert_eq!(table.retained_cell_count(), 6);
    assert!(
        matches!(&table.columns()[1], Column::Bool(values) if values == &[false, false, false])
    );
}

#[test]
fn add_column_accepts_the_exact_column_limit_and_rejects_before_mutation() {
    let limits = TableLimits::new(1, 2, 2);
    let mut database = Database::with_table_limits(limits);
    database
        .execute(
            "CREATE TABLE events (id Int64); \
             ALTER TABLE events ADD COLUMN label String; \
             INSERT INTO events VALUES (7, 'kept');",
        )
        .expect("two columns and two cells exactly fit both limits");

    assert_eq!(
        database.execute("ALTER TABLE events ADD COLUMN active Bool;"),
        Err(Error::ResourceLimitExceeded {
            resource: "table columns",
            actual: 3,
            max: 2,
        })
    );
    let table = database.catalog().table("events").expect("table remains");
    assert_eq!(table.limits(), limits);
    assert_eq!(table.schema().len(), 2);
    assert_eq!(table.columns().len(), 2);
    assert_eq!(table.retained_cell_count(), 2);
    assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &[7]));
    assert!(matches!(&table.columns()[1], Column::String(values) if values == &["kept"]));

    assert_eq!(
        database.execute("CREATE TABLE too_wide (a Int64, b Int64, c Int64);"),
        Err(Error::ResourceLimitExceeded {
            resource: "table columns",
            actual: 3,
            max: 2,
        })
    );
    assert!(!database.catalog().table_exists("too_wide"));
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
fn nullable_add_backfills_empty_and_populated_tables_and_defaults_omitted_inserts() {
    let mut empty = Database::new();
    empty
        .execute(
            "CREATE TABLE Empty (id Int64); \
             ALTER TABLE Empty ADD COLUMN measurement Nullable(Int64);",
        )
        .expect("nullable column can be added to an empty table");
    assert!(matches!(
        &empty.catalog().table("empty").unwrap().columns()[1],
        Column::NullableInt64(values) if values.is_empty()
    ));
    empty
        .execute(
            "INSERT INTO Empty (id) VALUES (1); \
             INSERT INTO Empty VALUES (2, 7);",
        )
        .expect("omitted nullable values default to NULL");
    assert_eq!(
        query(&mut empty, "SELECT id, measurement FROM Empty ORDER BY id",).rows,
        [
            vec![Value::Int64(1), Value::Null(DataType::Int64)],
            vec![Value::Int64(2), Value::Int64(7)],
        ]
    );

    let mut populated = Database::new();
    populated
        .execute(
            "CREATE TABLE Events (id Int64, label String); \
             INSERT INTO Events VALUES (2, 'two'), (1, 'one'); \
             ALTER TABLE Events ADD COLUMN measurement Nullable(Int64); \
             ALTER TABLE Events ADD COLUMN previous Nullable(Int64); \
             ALTER TABLE Events ADD COLUMN active Bool;",
        )
        .expect("nullable columns backfill populated rows");
    let table = populated.catalog().table("events").unwrap();
    assert!(matches!(
        &table.columns()[2],
        Column::NullableInt64(values) if values == &[None, None]
    ));
    assert!(matches!(
        &table.columns()[3],
        Column::NullableInt64(values) if values == &[None, None]
    ));
    assert_eq!(table.retained_value_bytes(), 60);

    populated
        .execute(
            "INSERT INTO Events (id, label, measurement) VALUES (3, 'three', 30); \
             INSERT INTO Events (id, label) VALUES (4, 'four');",
        )
        .expect("each omitted nullable column defaults to NULL");
    assert_eq!(
        query(
            &mut populated,
            "SELECT id, measurement, previous FROM Events ORDER BY id",
        )
        .rows,
        [
            vec![
                Value::Int64(1),
                Value::Null(DataType::Int64),
                Value::Null(DataType::Int64),
            ],
            vec![
                Value::Int64(2),
                Value::Null(DataType::Int64),
                Value::Null(DataType::Int64),
            ],
            vec![
                Value::Int64(3),
                Value::Int64(30),
                Value::Null(DataType::Int64),
            ],
            vec![
                Value::Int64(4),
                Value::Null(DataType::Int64),
                Value::Null(DataType::Int64),
            ],
        ]
    );

    let ddl = "CREATE TABLE Events (id Int64, label String); \
               ALTER TABLE Events ADD COLUMN measurement Nullable(Int64); \
               ALTER TABLE Events ADD COLUMN previous Nullable(Int64); \
               ALTER TABLE Events ADD COLUMN active Bool";
    assert_eq!(
        query(&mut populated, "SHOW CREATE TABLE events").rows,
        [vec![Value::String(ddl.to_owned())]]
    );
    assert_eq!(
        query(&mut populated, "DESCRIBE TABLE events").rows[2..4],
        [
            vec![
                Value::String("measurement".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
            ],
            vec![
                Value::String("previous".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
            ],
        ]
    );

    let mut replayed = Database::new();
    replayed
        .execute(ddl)
        .expect("SHOW CREATE output recreates every nullable column");
    assert_eq!(
        query(&mut replayed, "SHOW CREATE TABLE Events").rows,
        [vec![Value::String(ddl.to_owned())]]
    );
}

#[test]
fn conditional_nullable_add_is_idempotent_for_both_int64_physical_types() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Plain (measurement Int64); \
             INSERT INTO Plain VALUES (7), (9); \
             CREATE TABLE Maybe (measurement Nullable(Int64)); \
             INSERT INTO Maybe VALUES (3), (NULL);",
        )
        .expect("setup succeeds");

    for sql in [
        "ALTER TABLE plain ADD COLUMN IF NOT EXISTS Measurement Nullable(Int64)",
        "ALTER TABLE MAYBE ADD COLUMN IF NOT EXISTS MEASUREMENT Nullable(Int64)",
    ] {
        assert_eq!(
            database.execute(sql),
            Ok(vec![StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 0,
            }])
        );
    }

    let plain = database.catalog().table("Plain").unwrap();
    assert_eq!(plain.schema().len(), 1);
    assert!(matches!(&plain.columns()[0], Column::Int64(values) if values == &[7, 9]));
    let nullable = database.catalog().table("Maybe").unwrap();
    assert_eq!(nullable.schema().len(), 1);
    assert!(matches!(
        &nullable.columns()[0],
        Column::NullableInt64(values) if values == &[Some(3), None]
    ));
}

#[test]
fn conditional_nullable_add_backfills_populated_rows_and_show_create_replays() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Events (id Int64); \
             INSERT INTO Events VALUES (2), (1); \
             ALTER TABLE events ADD COLUMN IF NOT EXISTS measurement Nullable(Int64);",
        )
        .expect("the absent nullable column is added");

    let table = database.catalog().table("EVENTS").unwrap();
    assert_eq!(table.schema().len(), 2);
    assert!(matches!(
        &table.columns()[1],
        Column::NullableInt64(values) if values == &[None, None]
    ));

    let ddl = "CREATE TABLE Events (id Int64); \
               ALTER TABLE Events ADD COLUMN measurement Nullable(Int64)";
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE Events").rows,
        [vec![Value::String(ddl.to_owned())]]
    );
    let mut replayed = Database::new();
    replayed
        .execute(ddl)
        .expect("SHOW CREATE output remains executable");
    assert_eq!(
        query(&mut replayed, "SHOW CREATE TABLE events").rows,
        [vec![Value::String(ddl.to_owned())]]
    );
}

#[test]
fn nullable_add_accepts_exact_table_limits_and_rejects_atomically() {
    let limits = TableLimits::new(2, 3, 4);
    let mut database = Database::with_table_limits(limits);
    database
        .execute(
            "CREATE TABLE Events (id Int64); \
             INSERT INTO Events VALUES (1), (2); \
             ALTER TABLE Events ADD COLUMN IF NOT EXISTS measurement Nullable(Int64);",
        )
        .expect("two rows by two columns exactly fits four cells");
    let table = database.catalog().table("events").unwrap();
    assert_eq!(table.retained_cell_count(), limits.max_cells);
    assert!(matches!(
        &table.columns()[1],
        Column::NullableInt64(values) if values == &[None, None]
    ));

    assert_eq!(
        database.execute("ALTER TABLE Events ADD COLUMN IF NOT EXISTS overflow Nullable(Int64)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table cells",
            actual: 6,
            max: 4,
        })
    );
    let table = database.catalog().table("events").unwrap();
    assert_eq!(table.schema().len(), 2);
    assert_eq!(table.columns().len(), 2);
    assert_eq!(table.retained_cell_count(), 4);

    let mut columns = Database::with_table_limits(TableLimits::new(0, 2, 0));
    columns
        .execute(
            "CREATE TABLE Empty (id Int64); \
             ALTER TABLE Empty ADD COLUMN measurement Nullable(Int64);",
        )
        .expect("the exact column cap is accepted");
    assert_eq!(
        columns.execute("ALTER TABLE Empty ADD COLUMN overflow Nullable(Int64)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table columns",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(columns.catalog().table("empty").unwrap().schema().len(), 2);
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
        database.execute("ALTER TABLE missing ADD COLUMN IF NOT EXISTS score Nullable(Int64);"),
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
    let limits = TableLimits::new(2, 2, 4);
    let database = SharedDatabase::with_table_limits(limits);
    let other_handle = database.clone();
    assert_eq!(database.table_limits(), Ok(limits));
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
            retained_value_bytes: 16,
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
