use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, Statement, parse, parse_with_limits};
use rusthouse::batch::storage::{Column, ColumnDef, TableLimits};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).unwrap();
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result")
    };
    result.clone()
}

#[test]
fn parser_lowers_case_insensitive_nullable_int64_to_a_bounded_ast_shape() {
    for (sql, expected) in [
        (
            "CREATE TABLE readings (measurement Nullable(Int64))",
            Statement::CreateNullableInt64Table {
                name: "readings".to_owned(),
                column: "measurement".to_owned(),
            },
        ),
        (
            "cReAtE tAbLe Readings (Measurement nUlLaBlE ( iNt64 )) ENGINE = memory;",
            Statement::CreateNullableInt64Table {
                name: "Readings".to_owned(),
                column: "Measurement".to_owned(),
            },
        ),
        (
            "CREATE TABLE IF NOT EXISTS Readings (Measurement Nullable(Int64))",
            Statement::CreateNullableInt64TableIfNotExists {
                name: "Readings".to_owned(),
                column: "Measurement".to_owned(),
            },
        ),
    ] {
        assert_eq!(parse(sql), Ok(vec![expected]), "{sql:?}");
    }

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    assert!(parse_with_limits("CREATE TABLE t (c Nullable(Int64))", limits).is_ok());
    assert_eq!(
        parse_with_limits(
            "CREATE TABLE t (c Nullable(Int64))",
            BatchSqlLimits {
                max_ast_list_items: 0,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 1,
            max: 0,
        })
    );

    assert_eq!(
        parse("CREATE TABLE metrics (id Int64, label String, value Nullable(Int64))"),
        Ok(vec![Statement::CreateTableWithTrailingNullableInt64 {
            name: "metrics".to_owned(),
            columns: vec![
                ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
            nullable_column: "value".to_owned(),
        }])
    );
    assert_eq!(
        parse("CREATE TABLE IF NOT EXISTS metrics (id Bool, value Nullable(Int64))"),
        Ok(vec![
            Statement::CreateTableWithTrailingNullableInt64IfNotExists {
                name: "metrics".to_owned(),
                columns: vec![ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Bool,
                }],
                nullable_column: "value".to_owned(),
            },
        ])
    );

    let mixed = "CREATE TABLE t (id Int64, value Nullable(Int64))";
    assert!(
        parse_with_limits(
            mixed,
            BatchSqlLimits {
                max_ast_list_items: 2,
                ..BatchSqlLimits::default()
            },
        )
        .is_ok()
    );
    assert_eq!(
        parse_with_limits(
            mixed,
            BatchSqlLimits {
                max_ast_list_items: 1,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn parser_rejects_nullable_types_and_shapes_outside_the_bounded_forms() {
    for sql in [
        "CREATE TABLE t (c Nullable(Float64))",
        "CREATE TABLE t (c Nullable(Bool))",
        "CREATE TABLE t (c Nullable())",
        "CREATE TABLE t (c Nullable(Int64, Int64))",
        "CREATE TABLE t (c Nullable((Int64)))",
        "CREATE TABLE t (c Nullable Int64)",
        "CREATE TABLE t (c Nullable(Int64) NULL)",
        "CREATE TABLE t (c Nullable(Int64), d Int64)",
        "CREATE TABLE t (c Nullable(Int64), d Nullable(Int64))",
        "CREATE TABLE t (a Int64, b Nullable(Int64), c Int64)",
        "CREATE TABLE t (a Int64, b Nullable(Int64), c Nullable(Int64))",
    ] {
        assert!(
            parse(sql).is_err(),
            "out-of-shape CREATE was accepted: {sql:?}"
        );
    }
}

#[test]
fn trailing_nullable_create_supports_inserts_defaults_metadata_and_show_create_replay() {
    const DDL: &str = "CREATE TABLE Metrics (id Int64, label String, value Nullable(Int64))";
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Metrics (id Int64, label String, value Nullable(Int64)); \
             INSERT INTO metrics VALUES (1, 'present', 7), (2, 'absent', NULL); \
             INSERT INTO metrics (label, id) VALUES ('defaulted', 3);",
        )
        .expect("mixed CREATE and inserts succeed");

    let table = database.catalog().table("METRICS").unwrap();
    assert_eq!(table.limits(), TableLimits::default());
    assert!(matches!(
        &table.columns()[2],
        Column::NullableInt64(values) if values == &[Some(7), None, None]
    ));
    assert_eq!(
        query(
            &mut database,
            "SELECT id, label, value FROM metrics ORDER BY id"
        )
        .rows,
        [
            vec![
                Value::Int64(1),
                Value::String("present".to_owned()),
                Value::Int64(7),
            ],
            vec![
                Value::Int64(2),
                Value::String("absent".to_owned()),
                Value::Null(DataType::Int64),
            ],
            vec![
                Value::Int64(3),
                Value::String("defaulted".to_owned()),
                Value::Null(DataType::Int64),
            ],
        ]
    );
    assert_eq!(
        query(&mut database, "DESCRIBE TABLE metrics").rows,
        [
            vec![
                Value::String("id".to_owned()),
                Value::String("Int64".to_owned()),
            ],
            vec![
                Value::String("label".to_owned()),
                Value::String("String".to_owned()),
            ],
            vec![
                Value::String("value".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
            ],
        ]
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE metrics").rows,
        [vec![Value::String(DDL.to_owned())]]
    );

    let mut replayed = Database::new();
    replayed.execute(DDL).expect("SHOW CREATE output replays");
    replayed
        .execute("INSERT INTO Metrics (id, label) VALUES (4, 'replayed')")
        .expect("replayed nullable column retains its NULL default");
    assert!(matches!(
        &replayed.catalog().table("metrics").unwrap().columns()[2],
        Column::NullableInt64(values) if values == &[None]
    ));
}

#[test]
fn trailing_nullable_create_obeys_exact_column_row_and_cell_limits_atomically() {
    let limits = TableLimits::new(2, 2, 4);
    let mut exact = Database::with_table_limits(limits);
    exact
        .execute(
            "CREATE TABLE readings (id Int64, measurement Nullable(Int64)); \
             INSERT INTO readings VALUES (1, NULL), (2, 9);",
        )
        .expect("the exact column, row, and cell limits are accepted");
    let table = exact.catalog().table("readings").unwrap();
    assert_eq!(table.limits(), limits);
    assert_eq!(table.row_count(), limits.max_rows);
    assert_eq!(table.retained_cell_count(), limits.max_cells);
    assert_eq!(
        exact.execute("INSERT INTO readings VALUES (3, NULL)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 2,
        })
    );

    let mut column_limited = Database::with_table_limits(TableLimits::new(2, 1, 2));
    assert_eq!(
        column_limited.execute("CREATE TABLE rejected (id Int64, measurement Nullable(Int64))"),
        Err(Error::ResourceLimitExceeded {
            resource: "table columns",
            actual: 2,
            max: 1,
        })
    );
    assert_eq!(column_limited.catalog().table_count(), 0);

    let mut cell_limited = Database::with_table_limits(TableLimits::new(3, 2, 4));
    cell_limited
        .execute(
            "CREATE TABLE readings (id Int64, measurement Nullable(Int64)); \
             INSERT INTO readings VALUES (1, NULL), (2, 9);",
        )
        .unwrap();
    assert_eq!(
        cell_limited.execute("INSERT INTO readings VALUES (3, 10)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table cells",
            actual: 6,
            max: 4,
        })
    );
    assert_eq!(
        cell_limited
            .catalog()
            .table("readings")
            .unwrap()
            .row_count(),
        2
    );
}

#[test]
fn trailing_nullable_create_rejects_duplicate_names_without_partial_registration() {
    let mut database = Database::new();
    assert_eq!(
        database.execute("CREATE TABLE rejected (id Int64, ID Nullable(Int64))"),
        Err(Error::DuplicateColumn("ID".to_owned()))
    );
    assert_eq!(database.catalog().table_count(), 0);

    database
        .execute(
            "CREATE TABLE Metrics (id Int64, value Nullable(Int64)); \
             INSERT INTO Metrics VALUES (1, NULL);",
        )
        .unwrap();
    assert_eq!(
        database.execute("CREATE TABLE metrics (other Bool, replacement Nullable(Int64))"),
        Err(Error::TableAlreadyExists("metrics".to_owned()))
    );
    let table = database.catalog().table("METRICS").unwrap();
    assert_eq!(table.name(), "Metrics");
    assert_eq!(table.row_count(), 1);
    assert!(matches!(
        &table.columns()[1],
        Column::NullableInt64(values) if values == &[None]
    ));
}

#[test]
fn sql_create_uses_nullable_storage_and_round_trips_data_and_metadata() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (Measurement Nullable(Int64)); \
             INSERT INTO readings VALUES (7), (NULL), (-2);",
        )
        .unwrap();

    let table = database.catalog().table("READINGS").unwrap();
    let Column::NullableInt64(values) = &table.columns()[0] else {
        panic!("SQL nullable CREATE must use physical NullableInt64 storage")
    };
    assert_eq!(values, &[Some(7), None, Some(-2)]);

    assert_eq!(
        query(&mut database, "DESCRIBE TABLE readings").rows,
        [vec![
            Value::String("Measurement".to_owned()),
            Value::String("Nullable(Int64)".to_owned()),
        ]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT database, table, name, type, position FROM system.columns",
        )
        .rows,
        [vec![
            Value::String("default".to_owned()),
            Value::String("Readings".to_owned()),
            Value::String("Measurement".to_owned()),
            Value::String("Nullable(Int64)".to_owned()),
            Value::Int64(1),
        ]]
    );

    let ddl = "CREATE TABLE Readings (Measurement Nullable(Int64))";
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE readings"),
        QueryResult {
            columns: vec![ResultColumn {
                name: "statement".to_owned(),
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(ddl.to_owned())]],
        }
    );

    let mut recreated = Database::new();
    recreated.execute(ddl).unwrap();
    recreated
        .execute("INSERT INTO readings VALUES (NULL), (11)")
        .unwrap();
    let Column::NullableInt64(values) =
        &recreated.catalog().table("readings").unwrap().columns()[0]
    else {
        panic!("SHOW CREATE output must recreate nullable storage")
    };
    assert_eq!(values, &[None, Some(11)]);
}

#[test]
fn nullable_create_obeys_the_exact_one_column_table_limit() {
    let mut exact = Database::with_table_limits(TableLimits::new(0, 1, 0));
    exact
        .execute("CREATE TABLE readings (measurement Nullable(Int64))")
        .unwrap();

    let mut one_short = Database::with_table_limits(TableLimits::new(0, 0, 0));
    assert_eq!(
        one_short.execute("CREATE TABLE readings (measurement Nullable(Int64))"),
        Err(Error::ResourceLimitExceeded {
            resource: "table columns",
            actual: 1,
            max: 0,
        })
    );
    assert_eq!(one_short.catalog().table_count(), 0);
}

#[test]
fn conditional_nullable_create_preserves_the_existing_table() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (Measurement Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL); \
             CREATE TABLE IF NOT EXISTS READINGS (Replacement Nullable(Int64));",
        )
        .unwrap();

    let table = database.catalog().table("readings").unwrap();
    assert_eq!(table.name(), "Readings");
    assert_eq!(table.schema()[0].name, "Measurement");
    let Column::NullableInt64(values) = &table.columns()[0] else {
        panic!("existing nullable table must remain physical nullable storage")
    };
    assert_eq!(values, &[None]);
}

#[test]
fn conditional_trailing_nullable_create_is_a_case_insensitive_no_op() {
    let mut absent = Database::new();
    absent
        .execute(
            "CREATE TABLE IF NOT EXISTS Readings \
                 (id Int64, measurement Nullable(Int64))",
        )
        .expect("the conditional mixed form creates an absent table");
    assert!(matches!(
        &absent.catalog().table("readings").unwrap().columns()[1],
        Column::NullableInt64(values) if values.is_empty()
    ));

    let limits = TableLimits::new(1, 2, 2);
    let mut database = Database::with_table_limits(limits);
    database
        .execute(
            "CREATE TABLE Metrics (id Int64, value Nullable(Int64)); \
             INSERT INTO Metrics VALUES (1, NULL); \
             CREATE TABLE IF NOT EXISTS metrics \
                 (replacement String, ignored Nullable(Int64));",
        )
        .expect("an existing mixed table suppresses conditional creation");

    let table = database.catalog().table("METRICS").unwrap();
    assert_eq!(table.name(), "Metrics");
    assert_eq!(table.limits(), limits);
    assert_eq!(table.schema()[0].name, "id");
    assert_eq!(table.schema()[1].name, "value");
    assert!(matches!(
        &table.columns()[1],
        Column::NullableInt64(values) if values == &[None]
    ));
}
