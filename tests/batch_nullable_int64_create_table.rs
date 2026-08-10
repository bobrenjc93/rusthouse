use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, Statement, parse, parse_with_limits};
use rusthouse::batch::storage::{Column, TableLimits};
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
}

#[test]
fn parser_rejects_every_shape_outside_one_nullable_int64_column() {
    for sql in [
        "CREATE TABLE t (c Nullable(Float64))",
        "CREATE TABLE t (c Nullable(Bool))",
        "CREATE TABLE t (c Nullable())",
        "CREATE TABLE t (c Nullable(Int64, Int64))",
        "CREATE TABLE t (c Nullable((Int64)))",
        "CREATE TABLE t (c Nullable Int64)",
        "CREATE TABLE t (c Nullable(Int64) NULL)",
        "CREATE TABLE t (c Nullable(Int64), d Int64)",
        "CREATE TABLE t (d Int64, c Nullable(Int64))",
    ] {
        assert!(
            parse(sql).is_err(),
            "out-of-shape CREATE was accepted: {sql:?}"
        );
    }
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
