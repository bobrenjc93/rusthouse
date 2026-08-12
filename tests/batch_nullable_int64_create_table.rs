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
        (
            "CREATE TABLE readings (primary Nullable(Int64), backup Nullable(Int64))",
            Statement::CreateTableWithTwoTrailingNullableInt64 {
                name: "readings".to_owned(),
                columns: Vec::new(),
                nullable_columns: ["primary".to_owned(), "backup".to_owned()],
            },
        ),
        (
            "CREATE TABLE IF NOT EXISTS Readings (Primary nUlLaBlE(iNt64), Backup Nullable(Int64)) ENGINE = Memory",
            Statement::CreateTableWithTwoTrailingNullableInt64IfNotExists {
                name: "Readings".to_owned(),
                columns: Vec::new(),
                nullable_columns: ["Primary".to_owned(), "Backup".to_owned()],
            },
        ),
        (
            "CREATE TABLE readings (id Int64, measurement Nullable(Int64))",
            Statement::CreateTableWithTrailingNullableInt64 {
                name: "readings".to_owned(),
                columns: vec![ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                }],
                nullable_column: "measurement".to_owned(),
            },
        ),
        (
            "CREATE TABLE IF NOT EXISTS Readings (ID Int64, Label String, Measurement nUlLaBlE(iNt64)) ENGINE = Memory",
            Statement::CreateTableWithTrailingNullableInt64IfNotExists {
                name: "Readings".to_owned(),
                columns: vec![
                    ColumnDef {
                        name: "ID".to_owned(),
                        data_type: DataType::Int64,
                    },
                    ColumnDef {
                        name: "Label".to_owned(),
                        data_type: DataType::String,
                    },
                ],
                nullable_column: "Measurement".to_owned(),
            },
        ),
        (
            "CREATE TABLE readings (id Int64, primary Nullable(Int64), backup Nullable(Int64))",
            Statement::CreateTableWithTwoTrailingNullableInt64 {
                name: "readings".to_owned(),
                columns: vec![ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                }],
                nullable_columns: ["primary".to_owned(), "backup".to_owned()],
            },
        ),
        (
            "CREATE TABLE IF NOT EXISTS Readings (ID Int64, Label String, Primary nUlLaBlE(iNt64), Backup Nullable(Int64)) ENGINE = Memory",
            Statement::CreateTableWithTwoTrailingNullableInt64IfNotExists {
                name: "Readings".to_owned(),
                columns: vec![
                    ColumnDef {
                        name: "ID".to_owned(),
                        data_type: DataType::Int64,
                    },
                    ColumnDef {
                        name: "Label".to_owned(),
                        data_type: DataType::String,
                    },
                ],
                nullable_columns: ["Primary".to_owned(), "Backup".to_owned()],
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

    assert!(
        parse_with_limits(
            "CREATE TABLE t (id Int64, value Nullable(Int64))",
            BatchSqlLimits {
                max_ast_list_items: 2,
                ..BatchSqlLimits::default()
            },
        )
        .is_ok()
    );
    assert_eq!(
        parse_with_limits(
            "CREATE TABLE t (id Int64, value Nullable(Int64))",
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

    assert!(
        parse_with_limits(
            "CREATE TABLE t (primary Nullable(Int64), backup Nullable(Int64))",
            BatchSqlLimits {
                max_ast_list_items: 2,
                ..BatchSqlLimits::default()
            },
        )
        .is_ok()
    );
    assert_eq!(
        parse_with_limits(
            "CREATE TABLE t (primary Nullable(Int64), backup Nullable(Int64))",
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

    assert!(
        parse_with_limits(
            "CREATE TABLE t (id Int64, primary Nullable(Int64), backup Nullable(Int64))",
            BatchSqlLimits {
                max_ast_list_items: 3,
                ..BatchSqlLimits::default()
            },
        )
        .is_ok()
    );
    assert_eq!(
        parse_with_limits(
            "CREATE TABLE t (id Int64, primary Nullable(Int64), backup Nullable(Int64))",
            BatchSqlLimits {
                max_ast_list_items: 2,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 3,
            max: 2,
        })
    );
}

#[test]
fn parser_rejects_nullable_shapes_outside_the_bounded_trailing_form() {
    for sql in [
        "CREATE TABLE t (c Nullable(Float64))",
        "CREATE TABLE t (c Nullable(Bool))",
        "CREATE TABLE t (c Nullable())",
        "CREATE TABLE t (c Nullable(Int64, Int64))",
        "CREATE TABLE t (c Nullable((Int64)))",
        "CREATE TABLE t (c Nullable Int64)",
        "CREATE TABLE t (c Nullable(Int64) NULL)",
        "CREATE TABLE t (a Nullable(Int64), b Nullable(Int64), c Nullable(Int64))",
        "CREATE TABLE t (c Nullable(Int64), d Int64)",
        "CREATE TABLE t (a Int64, c Nullable(Int64), d Int64)",
        "CREATE TABLE t (a Int64, b Nullable(Int64), c Nullable(Int64), d Nullable(Int64))",
        "CREATE TABLE t (a Int64, b Nullable(Int64), c Nullable(Float64))",
    ] {
        assert!(
            parse(sql).is_err(),
            "out-of-shape CREATE was accepted: {sql:?}"
        );
    }
}

#[test]
fn two_all_nullable_create_stores_defaults_metadata_and_replayable_ddl() {
    const DDL: &str = "CREATE TABLE Readings (Primary Nullable(Int64), Backup Nullable(Int64))";
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (Primary Nullable(Int64), Backup Nullable(Int64)); \
             INSERT INTO readings VALUES (7, NULL), (NULL, 9); \
             INSERT INTO readings (Primary) VALUES (11); \
             INSERT INTO readings (Backup) VALUES (13);",
        )
        .unwrap();

    let [
        Column::NullableInt64(primary),
        Column::NullableInt64(backup),
    ] = database.catalog().table("READINGS").unwrap().columns()
    else {
        panic!("CREATE must publish two aligned nullable physical columns")
    };
    assert_eq!(primary, &[Some(7), None, Some(11), None]);
    assert_eq!(backup, &[None, Some(9), None, Some(13)]);

    assert_eq!(
        query(&mut database, "DESCRIBE TABLE readings").rows,
        [
            vec![
                Value::String("Primary".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
            ],
            vec![
                Value::String("Backup".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
            ],
        ]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT database, table, name, type, position FROM system.columns",
        )
        .rows,
        [
            vec![
                Value::String("default".to_owned()),
                Value::String("Readings".to_owned()),
                Value::String("Primary".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
                Value::Int64(1),
            ],
            vec![
                Value::String("default".to_owned()),
                Value::String("Readings".to_owned()),
                Value::String("Backup".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
                Value::Int64(2),
            ],
        ]
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE readings").rows,
        [vec![Value::String(DDL.to_owned())]]
    );

    let mut recreated = Database::new();
    recreated.execute(DDL).unwrap();
    recreated
        .execute("INSERT INTO readings (Backup) VALUES (5)")
        .unwrap();
    let [
        Column::NullableInt64(primary),
        Column::NullableInt64(backup),
    ] = recreated.catalog().table("readings").unwrap().columns()
    else {
        panic!("SHOW CREATE output must recreate both nullable columns")
    };
    assert_eq!(primary, &[None]);
    assert_eq!(backup, &[Some(5)]);
}

#[test]
fn two_trailing_nullable_create_stores_positional_and_subset_null_defaults_and_metadata() {
    const DDL: &str =
        "CREATE TABLE Readings (DeviceID Int64, Primary Nullable(Int64), Backup Nullable(Int64))";
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (DeviceID Int64, Primary Nullable(Int64), Backup Nullable(Int64)); \
             INSERT INTO readings VALUES (1, 7, NULL), (2, NULL, 9); \
             INSERT INTO readings (DeviceID) VALUES (3); \
             INSERT INTO readings (Backup, DeviceID) VALUES (11, 4);",
        )
        .unwrap();

    let table = database.catalog().table("READINGS").unwrap();
    let [
        Column::Int64(device_ids),
        Column::NullableInt64(primary),
        Column::NullableInt64(backup),
    ] = table.columns()
    else {
        panic!("CREATE must publish both aligned nullable physical columns")
    };
    assert_eq!(device_ids, &[1, 2, 3, 4]);
    assert_eq!(primary, &[Some(7), None, None, None]);
    assert_eq!(backup, &[None, Some(9), None, Some(11)]);

    assert_eq!(
        query(&mut database, "DESCRIBE TABLE readings").rows,
        [
            vec![
                Value::String("DeviceID".to_owned()),
                Value::String("Int64".to_owned()),
            ],
            vec![
                Value::String("Primary".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
            ],
            vec![
                Value::String("Backup".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
            ],
        ]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT database, table, name, type, position FROM system.columns",
        )
        .rows,
        [
            vec![
                Value::String("default".to_owned()),
                Value::String("Readings".to_owned()),
                Value::String("DeviceID".to_owned()),
                Value::String("Int64".to_owned()),
                Value::Int64(1),
            ],
            vec![
                Value::String("default".to_owned()),
                Value::String("Readings".to_owned()),
                Value::String("Primary".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
                Value::Int64(2),
            ],
            vec![
                Value::String("default".to_owned()),
                Value::String("Readings".to_owned()),
                Value::String("Backup".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
                Value::Int64(3),
            ],
        ]
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE readings").rows,
        [vec![Value::String(DDL.to_owned())]]
    );
    assert_eq!(parse(DDL).unwrap().len(), 1);

    let mut recreated = Database::new();
    recreated.execute(DDL).unwrap();
    recreated
        .execute("INSERT INTO readings (Primary, DeviceID) VALUES (5, 9)")
        .unwrap();
    let [
        _,
        Column::NullableInt64(primary),
        Column::NullableInt64(backup),
    ] = recreated.catalog().table("readings").unwrap().columns()
    else {
        panic!("SHOW CREATE output must recreate both nullable columns")
    };
    assert_eq!(primary, &[Some(5)]);
    assert_eq!(backup, &[None]);
}

#[test]
fn mixed_create_inserts_defaults_and_round_trips_storage_and_metadata() {
    const DDL: &str = "CREATE TABLE Readings (DeviceID Int64, Measurement Nullable(Int64))";
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (DeviceID Int64, Measurement Nullable(Int64)); \
             INSERT INTO readings VALUES (1, 7), (2, NULL); \
             INSERT INTO readings (DeviceID) VALUES (3);",
        )
        .unwrap();

    let table = database.catalog().table("READINGS").unwrap();
    let [
        Column::Int64(device_ids),
        Column::NullableInt64(measurements),
    ] = table.columns()
    else {
        panic!("mixed CREATE must use aligned Int64 and NullableInt64 storage")
    };
    assert_eq!(device_ids, &[1, 2, 3]);
    assert_eq!(measurements, &[Some(7), None, None]);

    assert_eq!(
        query(&mut database, "DESCRIBE TABLE readings").rows,
        [
            vec![
                Value::String("DeviceID".to_owned()),
                Value::String("Int64".to_owned()),
            ],
            vec![
                Value::String("Measurement".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
            ],
        ]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT database, table, name, type, position FROM system.columns",
        )
        .rows,
        [
            vec![
                Value::String("default".to_owned()),
                Value::String("Readings".to_owned()),
                Value::String("DeviceID".to_owned()),
                Value::String("Int64".to_owned()),
                Value::Int64(1),
            ],
            vec![
                Value::String("default".to_owned()),
                Value::String("Readings".to_owned()),
                Value::String("Measurement".to_owned()),
                Value::String("Nullable(Int64)".to_owned()),
                Value::Int64(2),
            ],
        ]
    );
    assert_eq!(
        query(&mut database, "SHOW CREATE TABLE readings").rows,
        [vec![Value::String(DDL.to_owned())]]
    );
    assert_eq!(parse(DDL).unwrap().len(), 1);

    let mut recreated = Database::new();
    recreated.execute(DDL).unwrap();
    recreated
        .execute("INSERT INTO readings (DeviceID) VALUES (9)")
        .unwrap();
    let Column::NullableInt64(values) =
        &recreated.catalog().table("readings").unwrap().columns()[1]
    else {
        panic!("SHOW CREATE output must recreate trailing nullable storage")
    };
    assert_eq!(values, &[None]);
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
fn mixed_create_obeys_exact_table_limits_without_partial_registration() {
    let ddl = "CREATE TABLE readings (id Int64, measurement Nullable(Int64))";
    let mut exact = Database::with_table_limits(TableLimits::new(1, 2, 2));
    exact.execute(ddl).unwrap();
    exact
        .execute("INSERT INTO readings VALUES (1, NULL)")
        .unwrap();
    assert_eq!(exact.catalog().table("readings").unwrap().row_count(), 1);

    let mut one_column_short = Database::with_table_limits(TableLimits::new(1, 1, 2));
    assert_eq!(
        one_column_short.execute(ddl),
        Err(Error::ResourceLimitExceeded {
            resource: "table columns",
            actual: 2,
            max: 1,
        })
    );
    assert_eq!(one_column_short.catalog().table_count(), 0);

    let mut one_cell_short = Database::with_table_limits(TableLimits::new(1, 2, 1));
    one_cell_short.execute(ddl).unwrap();
    assert_eq!(
        one_cell_short.execute("INSERT INTO readings VALUES (1, NULL)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table cells",
            actual: 2,
            max: 1,
        })
    );
    assert_eq!(
        one_cell_short
            .catalog()
            .table("readings")
            .unwrap()
            .row_count(),
        0
    );
}

#[test]
fn two_nullable_columns_obey_exact_column_and_cell_limits_atomically() {
    let ddl = "CREATE TABLE readings (id Int64, primary Nullable(Int64), backup Nullable(Int64))";
    let mut exact = Database::with_table_limits(TableLimits::new(1, 3, 3));
    exact.execute(ddl).unwrap();
    exact
        .execute("INSERT INTO readings VALUES (1, NULL, 2)")
        .unwrap();
    assert_eq!(exact.catalog().table("readings").unwrap().row_count(), 1);

    let mut one_column_short = Database::with_table_limits(TableLimits::new(1, 2, 3));
    assert_eq!(
        one_column_short.execute(ddl),
        Err(Error::ResourceLimitExceeded {
            resource: "table columns",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(one_column_short.catalog().table_count(), 0);

    let mut one_cell_short = Database::with_table_limits(TableLimits::new(1, 3, 2));
    one_cell_short.execute(ddl).unwrap();
    assert_eq!(
        one_cell_short.execute("INSERT INTO readings VALUES (1, NULL, NULL)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table cells",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(
        one_cell_short
            .catalog()
            .table("readings")
            .unwrap()
            .row_count(),
        0
    );
}

#[test]
fn two_all_nullable_columns_obey_exact_table_limits_atomically() {
    let ddl = "CREATE TABLE readings (primary Nullable(Int64), backup Nullable(Int64))";
    let mut exact = Database::with_table_limits(TableLimits::new(1, 2, 2));
    exact.execute(ddl).unwrap();
    exact
        .execute("INSERT INTO readings VALUES (NULL, 2)")
        .unwrap();
    assert_eq!(
        exact.execute("INSERT INTO readings VALUES (3, NULL)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 2,
            max: 1,
        })
    );
    assert_eq!(exact.catalog().table("readings").unwrap().row_count(), 1);

    let mut one_column_short = Database::with_table_limits(TableLimits::new(1, 1, 2));
    assert_eq!(
        one_column_short.execute(ddl),
        Err(Error::ResourceLimitExceeded {
            resource: "table columns",
            actual: 2,
            max: 1,
        })
    );
    assert_eq!(one_column_short.catalog().table_count(), 0);

    let mut one_cell_short = Database::with_table_limits(TableLimits::new(1, 2, 1));
    one_cell_short.execute(ddl).unwrap();
    assert_eq!(
        one_cell_short.execute("INSERT INTO readings VALUES (NULL, NULL)"),
        Err(Error::ResourceLimitExceeded {
            resource: "table cells",
            actual: 2,
            max: 1,
        })
    );
    assert_eq!(
        one_cell_short
            .catalog()
            .table("readings")
            .unwrap()
            .row_count(),
        0
    );
}

#[test]
fn mixed_create_rejects_duplicate_names_without_publishing_or_replacing() {
    let mut invalid = Database::new();
    assert_eq!(
        invalid.execute("CREATE TABLE readings (Value Int64, value Nullable(Int64))"),
        Err(Error::DuplicateColumn("value".to_owned()))
    );
    assert_eq!(invalid.catalog().table_count(), 0);

    let mut duplicate_nullable = Database::new();
    assert_eq!(
        duplicate_nullable.execute(
            "CREATE TABLE readings (id Int64, Measurement Nullable(Int64), measurement Nullable(Int64))"
        ),
        Err(Error::DuplicateColumn("measurement".to_owned()))
    );
    assert_eq!(duplicate_nullable.catalog().table_count(), 0);

    let mut duplicate_all_nullable = Database::new();
    assert_eq!(
        duplicate_all_nullable.execute(
            "CREATE TABLE readings (Measurement Nullable(Int64), measurement Nullable(Int64))"
        ),
        Err(Error::DuplicateColumn("measurement".to_owned()))
    );
    assert_eq!(duplicate_all_nullable.catalog().table_count(), 0);

    let mut duplicate_table = Database::new();
    duplicate_table
        .execute(
            "CREATE TABLE Readings (id Int64, measurement Nullable(Int64)); \
             INSERT INTO readings VALUES (1, NULL);",
        )
        .unwrap();
    assert_eq!(
        duplicate_table
            .execute("CREATE TABLE READINGS (replacement String, value Nullable(Int64))"),
        Err(Error::TableAlreadyExists("READINGS".to_owned()))
    );
    let table = duplicate_table.catalog().table("readings").unwrap();
    assert_eq!(table.name(), "Readings");
    assert_eq!(table.schema()[0].name, "id");
    assert_eq!(table.row_count(), 1);
}

#[test]
fn direct_mixed_create_ast_rejects_an_empty_non_nullable_prefix_without_panicking() {
    for statement in [
        Statement::CreateTableWithTrailingNullableInt64 {
            name: "plain".to_owned(),
            columns: Vec::new(),
            nullable_column: "value".to_owned(),
        },
        Statement::CreateTableWithTrailingNullableInt64IfNotExists {
            name: "conditional".to_owned(),
            columns: Vec::new(),
            nullable_column: "value".to_owned(),
        },
    ] {
        let mut database = Database::new();
        assert_eq!(
            database.execute_statement(statement),
            Err(Error::InvalidQuery(
                "a table must contain at least one column".to_owned()
            ))
        );
        assert_eq!(database.catalog().table_count(), 0);
    }
}

#[test]
fn conditional_two_all_nullable_create_is_a_case_insensitive_no_op() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE IF NOT EXISTS Readings (Primary Nullable(Int64), Backup Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL, 7); \
             CREATE TABLE IF NOT EXISTS READINGS (Other Nullable(Int64), other Nullable(Int64));",
        )
        .unwrap();

    let table = database.catalog().table("readings").unwrap();
    assert_eq!(table.name(), "Readings");
    assert_eq!(table.schema()[0].name, "Primary");
    assert_eq!(table.schema()[1].name, "Backup");
    let [
        Column::NullableInt64(primary),
        Column::NullableInt64(backup),
    ] = table.columns()
    else {
        panic!("conditional CREATE must preserve both nullable physical columns")
    };
    assert_eq!(primary, &[None]);
    assert_eq!(backup, &[Some(7)]);
}

#[test]
fn conditional_nullable_create_preserves_the_existing_table() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE IF NOT EXISTS Readings (ID Int64, Primary Nullable(Int64), Backup Nullable(Int64)); \
             INSERT INTO readings VALUES (1, NULL, 7); \
             CREATE TABLE IF NOT EXISTS READINGS (Replacement String, Other Nullable(Int64), OTHER Nullable(Int64));",
        )
        .unwrap();

    let table = database.catalog().table("readings").unwrap();
    assert_eq!(table.name(), "Readings");
    assert_eq!(table.schema()[0].name, "ID");
    assert_eq!(table.schema()[1].name, "Primary");
    assert_eq!(table.schema()[2].name, "Backup");
    let [
        _,
        Column::NullableInt64(primary),
        Column::NullableInt64(backup),
    ] = table.columns()
    else {
        panic!("existing nullable columns must remain physical nullable storage")
    };
    assert_eq!(primary, &[None]);
    assert_eq!(backup, &[Some(7)]);
}
