use rusthouse::{
    DEFAULT_TABLE_ROW_LIMIT, DataType, Database, MAX_SQL_INPUT_BYTES, ScalarValue, SqlErrorKind,
    write_csv,
};

#[test]
fn executes_create_table_alongside_scalar_selects() {
    let mut database = Database::new();

    let results = database
        .execute(
            "SELECT 7 AS before;\n\
             CREATE TABLE Metrics (id Int64, score Float64, active Bool, label String);\n\
             SELECT 'done' AS after;",
        )
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].header, "before");
    assert_eq!(results[0].value, ScalarValue::Integer(7));
    assert_eq!(results[1].header, "after");
    assert_eq!(results[1].value, ScalarValue::String("done".into()));

    let table = database.table("metrics").unwrap();
    assert_eq!(table.row_limit(), DEFAULT_TABLE_ROW_LIMIT);
    assert_eq!(table.row_count(), 0);
    assert_eq!(
        table
            .schema()
            .fields()
            .iter()
            .map(|field| (field.name(), field.data_type(), field.is_nullable()))
            .collect::<Vec<_>>(),
        vec![
            ("id", DataType::Int64, false),
            ("score", DataType::Float64, false),
            ("active", DataType::Bool, false),
            ("label", DataType::String, false),
        ]
    );
    assert!(database.table("METRICS").is_some());
    assert_eq!(database.table_count(), 1);
}

#[test]
fn create_table_produces_no_csv_result() {
    let mut database = Database::new();
    let results = database.execute("CREATE TABLE events (id Int64);").unwrap();
    let mut csv = Vec::new();

    write_csv(&results, &mut csv).unwrap();

    assert!(results.is_empty());
    assert!(csv.is_empty());
}

#[test]
fn comparisons_execute_alongside_catalog_changes() {
    let mut database = Database::new();

    let results = database
        .execute(
            "SELECT 2 = 2 AS equal;\n\
             CREATE TABLE events (id Int64);\n\
             SELECT NULL <> 'event' AS unknown;",
        )
        .unwrap();

    assert_eq!(results[0].value, ScalarValue::Boolean(true));
    assert_eq!(results[1].value, ScalarValue::Null);
    assert!(database.table("events").is_some());
}

#[test]
fn comparison_errors_roll_back_earlier_catalog_changes() {
    let mut database = database_with_seed();
    let before = database.clone();
    let sql = "CREATE TABLE fresh (id Int64); SELECT 1 = '1';";

    let error = database.execute(sql).unwrap_err();

    assert_eq!(error.byte_offset(), sql.find('=').unwrap());
    assert_eq!(error.column(), sql.find('=').unwrap() + 1);
    assert_eq!(
        error.kind(),
        &SqlErrorKind::Syntax {
            message: "operator '=' cannot compare Integer and String".into(),
        }
    );
    assert_eq!(database, before);
    assert!(database.table("fresh").is_none());
}

#[test]
fn duplicate_field_is_typed_positioned_and_preserves_catalog() {
    let mut database = database_with_seed();
    let before = database.clone();
    let sql = "SELECT 1;\nCREATE TABLE metrics (\n id Int64,\n ID String\n);";

    let error = database.execute(sql).unwrap_err();

    assert_eq!(
        error.kind(),
        &SqlErrorKind::DuplicateField {
            table: "metrics".into(),
            field: "ID".into(),
        }
    );
    assert_eq!(error.byte_offset(), sql.find("ID String").unwrap());
    assert_eq!((error.line(), error.column()), (4, 2));
    assert_eq!(database, before);
}

#[test]
fn duplicate_table_batch_is_atomic_and_case_insensitive() {
    let mut database = database_with_seed();
    let before = database.clone();
    let sql = "CREATE TABLE fresh (id Int64);\nCREATE TABLE FRESH (name String);";

    let error = database.execute(sql).unwrap_err();

    assert_eq!(
        error.kind(),
        &SqlErrorKind::DuplicateTable {
            table: "FRESH".into(),
        }
    );
    assert_eq!((error.line(), error.column()), (2, 14));
    assert_eq!(database, before);
    assert!(database.table("fresh").is_none());
}

#[test]
fn duplicate_existing_table_preserves_catalog() {
    let mut database = database_with_seed();
    let before = database.clone();

    let error = database
        .execute("CREATE TABLE SEED (other Bool);")
        .unwrap_err();

    assert_eq!(
        error.kind(),
        &SqlErrorKind::DuplicateTable {
            table: "SEED".into(),
        }
    );
    assert_eq!((error.line(), error.column()), (1, 14));
    assert_eq!(database, before);
}

#[test]
fn unknown_type_is_typed_positioned_and_preserves_catalog() {
    let mut database = database_with_seed();
    let before = database.clone();
    let sql = "CREATE TABLE readings (\n value Decimal\n);";

    let error = database.execute(sql).unwrap_err();

    assert_eq!(
        error.kind(),
        &SqlErrorKind::UnknownDataType {
            data_type: "Decimal".into(),
        }
    );
    assert_eq!(error.byte_offset(), sql.find("Decimal").unwrap());
    assert_eq!((error.line(), error.column()), (2, 8));
    assert_eq!(database, before);
}

#[test]
fn malformed_definitions_preserve_catalog() {
    for sql in [
        "CREATE TABLE missing_open id Int64);",
        "CREATE TABLE empty ();",
        "CREATE TABLE missing_type (id);",
        "CREATE TABLE trailing_comma (id Int64,);",
        "CREATE TABLE missing_comma (id Int64 name String);",
        "CREATE TABLE missing_end (id Int64)",
    ] {
        let mut database = database_with_seed();
        let before = database.clone();

        let error = database.execute(sql).unwrap_err();

        assert!(
            matches!(error.kind(), SqlErrorKind::Syntax { .. }),
            "SQL: {sql}, error: {error}"
        );
        assert_eq!(database, before, "SQL: {sql}");
    }
}

#[test]
fn enforces_the_sql_input_byte_limit_at_the_boundary() {
    assert_eq!(MAX_SQL_INPUT_BYTES, 32 * 1024 * 1024);

    let mut database = Database::new();
    let accepted_sql = padded_multibyte_sql(MAX_SQL_INPUT_BYTES);

    let results = database.execute(&accepted_sql).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].value, ScalarValue::Integer(1));

    let rejected_sql = padded_multibyte_sql(MAX_SQL_INPUT_BYTES + 1);
    let error = database.execute(&rejected_sql).unwrap_err();

    assert_eq!(error.byte_offset(), 0);
    assert_eq!((error.line(), error.column()), (1, 1));
    assert_eq!(
        error.kind(),
        &SqlErrorKind::InputTooLarge {
            max_bytes: MAX_SQL_INPUT_BYTES,
        }
    );
    assert!(database.is_empty());
}

fn padded_multibyte_sql(byte_len: usize) -> String {
    const PREFIX: &str = "SELECT 1;";
    const MULTIBYTE_WHITESPACE: &str = "\u{2003}";

    let padding_len = byte_len - PREFIX.len() - MULTIBYTE_WHITESPACE.len();
    let mut sql = String::with_capacity(byte_len);
    sql.push_str(PREFIX);
    sql.push_str(&" ".repeat(padding_len));
    sql.push_str(MULTIBYTE_WHITESPACE);
    assert_eq!(sql.len(), byte_len);
    assert!(sql.chars().count() < sql.len());
    sql
}

fn database_with_seed() -> Database {
    let mut database = Database::new();
    database.execute("CREATE TABLE seed (id Int64);").unwrap();
    database
}
