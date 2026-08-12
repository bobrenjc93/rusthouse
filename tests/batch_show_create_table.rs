use std::fmt::Write;
use std::mem::size_of;

use rusthouse::SharedDatabase;
use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{
    DEFAULT_MAX_AST_LIST_ITEMS, DEFAULT_MAX_BATCH_STATEMENTS, Statement, parse,
};
use rusthouse::batch::storage::{Column, ColumnDef, TableLimits};
use rusthouse::batch::value::{DataType, Value};

const CREATE: &str =
    "CREATE TABLE Metrics (EventID int64, Score FLOAT64, Active BOOLEAN, Label string);";
const DDL: &str = "CREATE TABLE Metrics (EventID Int64, Score Float64, Active Bool, Label String)";

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    result.clone()
}

fn result_bytes(ddl: &str) -> usize {
    size_of::<ResultColumn>()
        + "statement".len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>()
        + ddl.len()
}

#[test]
fn parses_exact_show_create_table_with_an_optional_semicolon() {
    for (sql, name) in [
        ("SHOW CREATE TABLE metrics", "metrics"),
        ("show create table Metrics;", "Metrics"),
    ] {
        assert_eq!(
            parse(sql).expect("valid SHOW CREATE TABLE"),
            [Statement::ShowCreateTable {
                name: name.to_owned(),
            }]
        );
    }

    assert!(matches!(
        parse("SHOW CREATE metrics"),
        Err(Error::Sql { .. })
    ));
    assert_eq!(
        parse("SHOW CREATE TABLE metrics extra"),
        Err(Error::Sql {
            position: 26,
            message: "unexpected trailing input after SHOW CREATE TABLE <name>".to_owned(),
        })
    );
}

#[test]
fn returns_canonical_ddl_in_schema_order_using_display_names() {
    let mut database = Database::new();
    database.execute(CREATE).expect("setup succeeds");

    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE mEtRiCs;"),
        QueryResult {
            columns: vec![ResultColumn {
                name: "statement".to_owned(),
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(DDL.to_owned())]],
        }
    );
    assert_eq!(
        database.execute("SHOW CREATE TABLE absent;"),
        Err(Error::TableNotFound("absent".to_owned()))
    );
}

#[test]
fn accepts_exact_and_rejects_exceeded_result_limits() {
    let exact_bytes = result_bytes(DDL);
    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(CREATE).expect("setup succeeds");
    assert_eq!(
        query(&mut exact, "SHOW CREATE TABLE metrics").rows,
        [vec![Value::String(DDL.to_owned())]]
    );

    let cases = [
        (
            QueryResultLimits {
                max_rows: 0,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW CREATE TABLE result rows",
                actual: 1,
                max: 0,
            },
        ),
        (
            QueryResultLimits {
                max_values: 0,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW CREATE TABLE result values",
                actual: 1,
                max: 0,
            },
        ),
        (
            QueryResultLimits {
                max_bytes: exact_bytes - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "SHOW CREATE TABLE result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            },
        ),
    ];

    for (limits, expected) in cases {
        let mut database = Database::with_query_result_limits(limits);
        database.execute(CREATE).expect("setup succeeds");
        assert_eq!(database.execute("SHOW CREATE TABLE metrics"), Err(expected));
    }

    let mut retained = Database::new();
    retained.execute(CREATE).expect("setup succeeds");
    assert!(
        retained
            .execute_with_result_limit("SHOW CREATE TABLE metrics", exact_bytes)
            .is_ok()
    );
    assert_eq!(
        retained.execute_with_result_limit("SHOW CREATE TABLE metrics", exact_bytes - 1),
        Err(Error::ResultLimitExceeded {
            bytes: exact_bytes,
            max_bytes: exact_bytes - 1,
        })
    );
}

#[test]
fn nullable_int64_ddl_obeys_the_exact_show_create_byte_boundary() {
    const NULLABLE_DDL: &str = "CREATE TABLE Readings (Measurement Nullable(Int64))";
    let exact_bytes = result_bytes(NULLABLE_DDL);

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(NULLABLE_DDL).expect("setup succeeds");
    assert_eq!(
        query(&mut exact, "SHOW CREATE TABLE readings").rows,
        [vec![Value::String(NULLABLE_DDL.to_owned())]]
    );

    let mut one_short = Database::with_query_result_limits(QueryResultLimits {
        max_bytes: exact_bytes - 1,
        ..QueryResultLimits::default()
    });
    one_short.execute(NULLABLE_DDL).expect("setup succeeds");
    assert_eq!(
        one_short.execute("SHOW CREATE TABLE readings"),
        Err(Error::ResourceLimitExceeded {
            resource: "SHOW CREATE TABLE result bytes",
            actual: exact_bytes,
            max: exact_bytes - 1,
        })
    );
}

#[test]
fn mixed_nullable_schema_show_create_replays_create_and_alter_at_exact_byte_limit() {
    const SETUP: &str = "CREATE TABLE Mixed (v Nullable(Int64)); \
                         ALTER TABLE Mixed ADD COLUMN tag String; \
                         ALTER TABLE Mixed ADD COLUMN active Bool;";
    const REPLAYABLE_DDL: &str = "CREATE TABLE Mixed (v Nullable(Int64)); \
                                  ALTER TABLE Mixed ADD COLUMN tag String; \
                                  ALTER TABLE Mixed ADD COLUMN active Bool";
    let exact_bytes = result_bytes(REPLAYABLE_DDL);
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    database.execute(SETUP).expect("setup succeeds");

    let shown = query(&mut database, "SHOW CREATE TABLE mixed");
    assert_eq!(shown.rows, [vec![Value::String(REPLAYABLE_DDL.to_owned())]]);
    assert_eq!(parse(REPLAYABLE_DDL).unwrap().len(), 3);

    let mut recreated = Database::new();
    recreated
        .execute(REPLAYABLE_DDL)
        .expect("SHOW CREATE output is executable");
    assert_eq!(query(&mut recreated, "SHOW CREATE TABLE MIXED"), shown);
    recreated
        .execute("INSERT INTO mixed VALUES (NULL, 'kept', true)")
        .expect("recreated nullable storage accepts NULL");
    assert_eq!(
        query(&mut recreated, "DESCRIBE TABLE mixed").rows,
        [
            vec![
                Value::String("v".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
            ],
            vec![
                Value::String("tag".to_owned()),
                Value::String("String".to_owned()),
            ],
            vec![
                Value::String("active".to_owned()),
                Value::String("Bool".to_owned()),
            ],
        ]
    );

    let mut one_short = Database::with_query_result_limits(QueryResultLimits {
        max_bytes: exact_bytes - 1,
        ..QueryResultLimits::default()
    });
    one_short.execute(SETUP).expect("setup succeeds");
    assert_eq!(
        one_short.execute("SHOW CREATE TABLE mixed"),
        Err(Error::ResourceLimitExceeded {
            resource: "SHOW CREATE TABLE result bytes",
            actual: exact_bytes,
            max: exact_bytes - 1,
        })
    );
}

#[test]
fn nullable_show_create_replay_accepts_4096_and_rejects_4097_statements() {
    let limits = TableLimits::new(0, DEFAULT_MAX_BATCH_STATEMENTS + 1, 0);
    let mut database = Database::with_table_limits(limits);
    let mut setup = String::from("CREATE TABLE Boundary (c0 Int64);");
    for index in 1..DEFAULT_MAX_BATCH_STATEMENTS {
        let data_type = if index == 1 {
            "Nullable(Int64)"
        } else {
            "Int64"
        };
        write!(
            setup,
            "ALTER TABLE Boundary ADD COLUMN c{index} {data_type};"
        )
        .unwrap();
    }
    database
        .execute(&setup)
        .expect("the exact replay statement limit is accepted");

    let shown = query(&mut database, "SHOW CREATE TABLE boundary");
    let [Value::String(ddl)] = shown.rows[0].as_slice() else {
        panic!("SHOW CREATE returns one DDL string")
    };
    assert_eq!(parse(ddl).unwrap().len(), DEFAULT_MAX_BATCH_STATEMENTS);

    let mut recreated = Database::with_table_limits(limits);
    recreated
        .execute(ddl)
        .expect("the exact-limit SHOW CREATE output is replayable");
    assert_eq!(
        recreated
            .catalog()
            .table("boundary")
            .unwrap()
            .schema()
            .len(),
        DEFAULT_MAX_BATCH_STATEMENTS
    );

    assert_eq!(
        database.execute(&format!(
            "ALTER TABLE Boundary ADD COLUMN c{} Nullable(Int64)",
            DEFAULT_MAX_BATCH_STATEMENTS
        )),
        Err(Error::ResourceLimitExceeded {
            resource: "nullable SHOW CREATE statements",
            actual: DEFAULT_MAX_BATCH_STATEMENTS + 1,
            max: DEFAULT_MAX_BATCH_STATEMENTS,
        })
    );
    assert_eq!(
        database.catalog().table("boundary").unwrap().schema().len(),
        DEFAULT_MAX_BATCH_STATEMENTS
    );
}

#[test]
fn trailing_nullable_show_create_coalesces_only_within_the_ast_list_boundary() {
    fn int64_columns(count: usize) -> Vec<ColumnDef> {
        (0..count)
            .map(|index| ColumnDef {
                name: format!("c{index}"),
                data_type: DataType::Int64,
            })
            .collect()
    }

    {
        let limits = TableLimits::new(0, DEFAULT_MAX_AST_LIST_ITEMS, 0);
        let mut database = Database::with_table_limits(limits);
        database
            .execute_statement(Statement::CreateTable {
                name: "Exact".to_owned(),
                columns: int64_columns(DEFAULT_MAX_AST_LIST_ITEMS - 1),
            })
            .expect("the non-nullable prefix is valid");
        database
            .execute("ALTER TABLE Exact ADD COLUMN trailing Nullable(Int64)")
            .expect("the trailing nullable column reaches the exact AST-list boundary");

        let shown = query(&mut database, "SHOW CREATE TABLE Exact");
        let [Value::String(ddl)] = shown.rows[0].as_slice() else {
            panic!("SHOW CREATE returns one DDL string")
        };
        assert_eq!(
            parse(ddl)
                .expect("the exact-boundary DDL is parseable")
                .len(),
            1
        );

        let mut replayed = Database::with_table_limits(limits);
        replayed
            .execute(ddl)
            .expect("the exact-boundary coalesced CREATE replays");
        assert!(matches!(
            replayed.catalog().table("exact").unwrap().columns().last(),
            Some(Column::NullableInt64(values)) if values.is_empty()
        ));
    }

    let limits = TableLimits::new(0, DEFAULT_MAX_AST_LIST_ITEMS + 1, 0);
    let mut database = Database::with_table_limits(limits);
    database
        .execute_statement(Statement::CreateTable {
            name: "Overflow".to_owned(),
            columns: int64_columns(DEFAULT_MAX_AST_LIST_ITEMS),
        })
        .expect("the exact-boundary non-nullable prefix is valid");
    database
        .execute("ALTER TABLE Overflow ADD COLUMN trailing Nullable(Int64)")
        .expect("the trailing nullable column may exceed the CREATE AST-list boundary");

    let shown = query(&mut database, "SHOW CREATE TABLE Overflow");
    let [Value::String(ddl)] = shown.rows[0].as_slice() else {
        panic!("SHOW CREATE returns one DDL string")
    };
    let statements = parse(ddl).expect("the split DDL remains parseable at the boundary");
    assert_eq!(statements.len(), 2);
    assert!(matches!(
        statements.as_slice(),
        [
            Statement::CreateTable { columns, .. },
            Statement::AddNullableInt64Column { column, .. },
        ] if columns.len() == DEFAULT_MAX_AST_LIST_ITEMS && column == "trailing"
    ));

    let mut replayed = Database::with_table_limits(limits);
    replayed
        .execute(ddl)
        .expect("the exact-boundary CREATE plus nullable ALTER replays");
    let replayed = replayed.catalog().table("overflow").unwrap();
    assert_eq!(replayed.schema().len(), DEFAULT_MAX_AST_LIST_ITEMS + 1);
    assert!(matches!(
        replayed.columns().last(),
        Some(Column::NullableInt64(values)) if values.is_empty()
    ));
}

#[test]
fn shared_database_reads_show_create_table_under_a_read_lock() {
    let database = SharedDatabase::default();
    database.execute(CREATE).expect("setup succeeds");

    assert_eq!(
        database
            .query("SHOW CREATE TABLE METRICS;")
            .expect("SHOW CREATE TABLE is read-only")
            .rows,
        [vec![Value::String(DDL.to_owned())]]
    );
}

#[test]
fn direct_ast_create_validates_identifiers_and_round_trips_show_create_sql() {
    let statement = Statement::CreateTable {
        name: "Manual_Table1".to_owned(),
        columns: vec![
            ColumnDef {
                name: "EventID".to_owned(),
                data_type: DataType::Int64,
            },
            ColumnDef {
                name: "score_2".to_owned(),
                data_type: DataType::Float64,
            },
            ColumnDef {
                name: "Active".to_owned(),
                data_type: DataType::Bool,
            },
            ColumnDef {
                name: "Label".to_owned(),
                data_type: DataType::String,
            },
        ],
    };
    let mut database = Database::new();
    assert_eq!(
        database.execute_statement(statement.clone()),
        Ok(StatementResult::Command {
            tag: "CREATE TABLE",
            affected_rows: 0,
        })
    );

    let result = query(&mut database, "SHOW CREATE TABLE manual_table1");
    let [row] = result.rows.as_slice() else {
        panic!("SHOW CREATE TABLE must return one row");
    };
    let [Value::String(ddl)] = row.as_slice() else {
        panic!("SHOW CREATE TABLE must return one String");
    };
    assert_eq!(parse(ddl), Ok(vec![statement]));

    let mut round_tripped = Database::new();
    round_tripped.execute(ddl).expect("shown DDL is executable");
    assert_eq!(
        query(&mut round_tripped, "SHOW CREATE TABLE MANUAL_TABLE1"),
        result
    );

    for (name, column, expected) in [
        (
            "bad name",
            "id",
            Error::InvalidIdentifier {
                identifier: "bad name".to_owned(),
                context: "table name".to_owned(),
            },
        ),
        (
            "valid_name",
            "id)",
            Error::InvalidIdentifier {
                identifier: "id)".to_owned(),
                context: "column name".to_owned(),
            },
        ),
    ] {
        let invalid = Statement::CreateTable {
            name: name.to_owned(),
            columns: vec![ColumnDef {
                name: column.to_owned(),
                data_type: DataType::Int64,
            }],
        };
        assert_eq!(database.execute_statement(invalid), Err(expected));
    }
    assert_eq!(database.catalog().table_count(), 1);
}
