use rusthouse::{DataType, Database, DatabaseConfig, Error, Schema, Table};

#[test]
fn creates_a_harness_shaped_schema_with_case_insensitive_keywords_and_types() {
    let mut database = Database::new();
    database
        .execute(
            "cReAtE tAbLe parity_data (
                id iNt64, uniform_num Int64, skewed_num Int64, large_int Int64,
                aux_int_a Int64, aux_int_b Int64, score fLoAt64,
                aux_score Float64, bucket Int64, low_key STRING,
                high_key String, payload String, region String, code String,
                description String, label String, flag bOoL, secondary_flag Bool
            );",
        )
        .expect("valid CREATE TABLE");

    let table = database
        .catalog()
        .table("PARITY_DATA")
        .expect("registered table");
    assert_eq!(table.name(), "parity_data");
    assert_eq!(table.len(), 18);
    assert_eq!(table.column("ID").expect("id").data_type(), DataType::Int64);
    assert_eq!(
        table.column("score").expect("score").data_type(),
        DataType::Float64
    );
    assert_eq!(
        table.column("flag").expect("flag").data_type(),
        DataType::Bool
    );
    assert_eq!(
        table.column("payload").expect("payload").data_type(),
        DataType::String
    );
}

#[test]
fn created_schema_initializes_the_typed_columnar_table() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (UserID Int64, score Float64, active Bool, label String)")
        .expect("valid CREATE TABLE");

    let table_schema = database
        .catalog()
        .table("events")
        .expect("registered table");
    assert_eq!(
        table_schema
            .column("userid")
            .expect("case-insensitive catalog column")
            .data_type(),
        DataType::Int64
    );

    let mut table = Table::new(Schema::from(table_schema));
    table
        .insert_rows(vec![vec![
            1.into(),
            2.5.into(),
            true.into(),
            "first".into(),
        ]])
        .expect("schema types are shared with storage");

    assert_eq!(table.row_count(), 1);
    assert_eq!(
        table
            .column_by_name("uSeRiD")
            .expect("case-insensitive storage column")
            .as_int64(),
        Some([1].as_slice())
    );
}

#[test]
fn rejects_duplicate_tables_without_replacing_the_original_schema() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE Events (id Int64)")
        .expect("first table");

    assert_eq!(
        database.execute("create table events (label String)"),
        Err(Error::TableAlreadyExists {
            name: "events".to_owned()
        })
    );
    let table = database.catalog().table("events").expect("original table");
    assert_eq!(table.columns()[0].name(), "id");
}

#[test]
fn rejects_case_insensitive_duplicate_columns_and_unknown_types() {
    let mut database = Database::new();

    assert_eq!(
        database.execute("CREATE TABLE duplicate (id Int64, ID String)"),
        Err(Error::DuplicateColumn {
            name: "ID".to_owned()
        })
    );
    assert_eq!(
        database.execute("CREATE TABLE unknown (value UInt64)"),
        Err(Error::UnknownType {
            name: "UInt64".to_owned(),
            position: 28
        })
    );
    assert!(database.catalog().is_empty());
}

#[test]
fn enforces_input_and_column_limits_at_the_boundary() {
    let two_columns = "CREATE TABLE ok (a Int64, b Bool)";
    let mut database = Database::with_config(DatabaseConfig::new(two_columns.len(), 2));
    database.execute(two_columns).expect("limits are inclusive");

    let oversized = format!("{two_columns} ");
    assert_eq!(
        database.execute(&oversized),
        Err(Error::InputTooLarge {
            actual: oversized.len(),
            maximum: two_columns.len()
        })
    );

    let mut database = Database::with_config(DatabaseConfig::new(1024, 2));
    assert_eq!(
        database.execute("CREATE TABLE wide (a Int64, b Float64, c String)"),
        Err(Error::TooManyColumns {
            actual: 3,
            maximum: 2
        })
    );
    assert!(database.catalog().is_empty());
}

#[test]
fn malformed_or_multiple_statements_do_not_change_the_catalog() {
    let malformed = [
        "",
        "CREATE events (id Int64)",
        "CREATE TABLE events ()",
        "CREATE TABLE events (id)",
        "CREATE TABLE events (id Int64,)",
        "CREATE TABLE events id Int64)",
        "CREATE TABLE events (id Int64",
        "CREATE TABLE events (id Int64) trailing",
        "CREATE TABLE first (id Int64); CREATE TABLE second (id Int64)",
        "DROP TABLE events",
    ];

    for input in malformed {
        let mut database = Database::new();
        assert!(
            database.execute(input).is_err(),
            "input unexpectedly passed: {input}"
        );
        assert!(
            database.catalog().is_empty(),
            "input changed catalog: {input}"
        );
    }
}
