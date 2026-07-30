use rusthouse::{DataType, Database, Error, QueryResult, ResultColumn, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("SQL succeeds");
    let Some(StatementResult::Query(result)) = results.last() else {
        panic!("expected a query result");
    };
    result.clone()
}

#[test]
fn qualified_statements_support_identical_table_names() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE DATABASE Analytics;
             CREATE DATABASE Archive;
             CREATE TABLE analytics.events (id Int64, label String);
             CREATE TABLE ARCHIVE.EVENTS (id Int64, label String);
             INSERT INTO ANALYTICS.Events VALUES (1, 'live');
             INSERT INTO archive.events VALUES (2, 'old');",
        )
        .expect("qualified setup succeeds");

    let analytics = query(&mut database, "SELECT id, label FROM aNaLyTiCs.eVeNtS;");
    let archive = query(&mut database, "SELECT id, label FROM Archive.Events;");

    assert_eq!(
        analytics.rows,
        vec![vec![Value::Int64(1), Value::String("live".to_owned())]]
    );
    assert_eq!(
        archive.rows,
        vec![vec![Value::Int64(2), Value::String("old".to_owned())]]
    );
    assert_eq!(database.current_database(), "default");
}

#[test]
fn use_changes_unqualified_resolution_across_execute_calls() {
    let mut database = Database::new();
    assert_eq!(database.current_database(), "default");

    database.execute("CREATE DATABASE Reporting;").unwrap();
    database.execute("USE reporting;").unwrap();
    assert_eq!(database.current_database(), "Reporting");

    database
        .execute("CREATE TABLE totals (amount Int64);")
        .unwrap();
    database
        .execute("INSERT INTO totals VALUES (7), (3);")
        .unwrap();
    database
        .execute("CREATE TABLE default.totals (amount Int64);")
        .unwrap();
    database
        .execute("INSERT INTO DEFAULT.TOTALS VALUES (99);")
        .unwrap();

    let active = query(&mut database, "SELECT amount FROM totals ORDER BY amount;");
    let qualified_default = query(&mut database, "SELECT amount FROM default.totals;");
    assert_eq!(
        active.rows,
        vec![vec![Value::Int64(3)], vec![Value::Int64(7)]]
    );
    assert_eq!(qualified_default.rows, vec![vec![Value::Int64(99)]]);
    assert_eq!(database.current_database(), "Reporting");

    let error = database
        .execute("USE missing;")
        .expect_err("unknown database is rejected");
    assert_eq!(error, Error::DatabaseNotFound("missing".to_owned()));
    assert_eq!(database.current_database(), "Reporting");
}

#[test]
fn active_and_nonempty_databases_cannot_be_dropped() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE DATABASE empty_active;
             CREATE DATABASE occupied;
             CREATE TABLE occupied.events (id Int64);
             USE empty_active;",
        )
        .unwrap();

    assert_eq!(
        database.execute("DROP DATABASE EMPTY_ACTIVE;"),
        Err(Error::DatabaseIsActive("EMPTY_ACTIVE".to_owned()))
    );
    assert_eq!(
        database.execute("DROP DATABASE OCCUPIED;"),
        Err(Error::DatabaseNotEmpty("occupied".to_owned()))
    );

    database.execute("USE default;").unwrap();
    database.execute("DROP DATABASE empty_active;").unwrap();
    assert_eq!(
        database.execute("USE empty_active;"),
        Err(Error::DatabaseNotFound("empty_active".to_owned()))
    );
}

#[test]
fn database_discovery_is_deterministic_and_preserves_spelling() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE DATABASE zeta;
             CREATE DATABASE Analytics;
             CREATE DATABASE beta;",
        )
        .unwrap();

    let expected = QueryResult {
        columns: vec![ResultColumn {
            name: "name".to_owned(),
            data_type: DataType::String,
        }],
        rows: vec![
            vec![Value::String("Analytics".to_owned())],
            vec![Value::String("beta".to_owned())],
            vec![Value::String("default".to_owned())],
            vec![Value::String("zeta".to_owned())],
        ],
    };

    assert_eq!(query(&mut database, "SHOW DATABASES;"), expected);
    assert_eq!(query(&mut database, "show databases;"), expected);

    assert_eq!(
        database.execute("CREATE DATABASE ANALYTICS;"),
        Err(Error::DatabaseAlreadyExists("ANALYTICS".to_owned()))
    );
}

#[test]
fn qualification_errors_are_specific_and_do_not_mutate_the_catalog() {
    let mut database = Database::new();

    for sql in [
        "CREATE TABLE absent.events (id Int64);",
        "INSERT INTO absent.events VALUES (1);",
        "SELECT * FROM absent.events;",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::DatabaseNotFound("absent".to_owned()))
        );
    }

    database.execute("CREATE DATABASE warehouse;").unwrap();
    assert_eq!(
        database.execute("SELECT * FROM warehouse.missing;"),
        Err(Error::TableNotFound("warehouse.missing".to_owned()))
    );

    let error = database
        .execute("CREATE TABLE cluster.warehouse.events (id Int64);")
        .expect_err("three-part names are unsupported");
    assert!(matches!(
        error,
        Error::Sql { message, .. }
            if message == "table names may contain at most one database qualifier"
    ));
    assert!(database.catalog().database("cluster").is_err());
}
