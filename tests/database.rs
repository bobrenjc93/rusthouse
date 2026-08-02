use rusthouse::{
    BatchAppendError, Column, DEFAULT_TABLE_ROW_LIMIT, DataType, Database, MAX_IDENTIFIER_BYTES,
    MAX_SCHEMA_FIELDS, MAX_STORED_STRING_BYTES, ScalarValue, SchemaError, SqlErrorKind, ValueType,
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
fn positional_insert_stores_all_types_in_schema_order_and_produces_no_csv() {
    let mut database = Database::new();

    let results = database
        .execute(
            "CREATE TABLE events (id Int64, score Float64, active Bool, label String);\n\
             INSERT INTO EVENTS VALUES\n\
             (1, 1.5, TRUE, 'it''s ready'),\n\
             (2, -0.25, FALSE, 'second');",
        )
        .unwrap();

    assert!(results.is_empty());
    let table = database.table("events").unwrap();
    assert_eq!(table.row_count(), 2);
    let Column::Int64(ids) = table.column("id").unwrap() else {
        panic!("id should be an Int64 column");
    };
    assert_eq!(ids.values(), &[1, 2]);
    let Column::Float64(scores) = table.column("score").unwrap() else {
        panic!("score should be a Float64 column");
    };
    assert_eq!(scores.values(), &[1.5, -0.25]);
    let Column::Bool(active) = table.column("active").unwrap() else {
        panic!("active should be a Bool column");
    };
    assert_eq!(active.values(), &[true, false]);
    let Column::String(labels) = table.column("label").unwrap() else {
        panic!("label should be a String column");
    };
    assert_eq!(labels.values(), &["it's ready", "second"]);
}

#[test]
fn late_invalid_insert_row_is_typed_and_rolls_back_the_complete_batch() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, score Float64, active Bool, label String);\n\
             INSERT INTO events VALUES (1, 1.0, TRUE, 'existing');",
        )
        .unwrap();
    let before = database.clone();
    let sql = "INSERT INTO events VALUES\n\
               (2, 2.0, FALSE, 'valid'),\n\
               (3, 'wrong', TRUE, 'invalid');";

    let error = database.execute(sql).unwrap_err();

    assert_eq!(
        error.kind(),
        &SqlErrorKind::InvalidRow {
            table: "events".into(),
            source: BatchAppendError::TypeMismatch {
                row_index: 1,
                field: "score".into(),
                expected: DataType::Float64,
                actual: ValueType::String,
            },
        }
    );
    assert_eq!((error.line(), error.column()), (3, 1));
    assert_eq!(database, before);
}

#[test]
fn unknown_insert_table_is_typed_and_rolls_back_earlier_statements() {
    let mut database = database_with_seed();
    let before = database.clone();
    let sql = "INSERT INTO seed VALUES (2);\n\
               CREATE TABLE fresh (id Int64);\n\
               INSERT INTO missing VALUES (1);";

    let error = database.execute(sql).unwrap_err();

    assert_eq!(
        error.kind(),
        &SqlErrorKind::UnknownTable {
            table: "missing".into(),
        }
    );
    assert_eq!((error.line(), error.column()), (3, 13));
    assert_eq!(database, before);
    assert!(database.table("fresh").is_none());
}

#[test]
fn oversized_insert_string_is_typed_positioned_and_rolls_back_the_batch() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE messages (value String); INSERT INTO messages VALUES ('existing');")
        .unwrap();
    let before = database.clone();
    let oversized = format!("{}a", "é".repeat(MAX_STORED_STRING_BYTES / "é".len()));
    let sql = format!("INSERT INTO messages VALUES\n('valid'),\n('{oversized}');");

    let error = database.execute(&sql).unwrap_err();

    assert_eq!(
        error.kind(),
        &SqlErrorKind::InvalidRow {
            table: "messages".into(),
            source: BatchAppendError::StringTooLong {
                row_index: 1,
                field: "value".into(),
                length: MAX_STORED_STRING_BYTES + 1,
                limit: MAX_STORED_STRING_BYTES,
            },
        }
    );
    assert_eq!((error.line(), error.column()), (3, 1));
    assert_eq!(database, before);
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
fn schema_limit_errors_are_typed_positioned_and_preserve_catalog() {
    let long_field = "a".repeat(MAX_IDENTIFIER_BYTES + 1);
    let definitions = (0..=MAX_SCHEMA_FIELDS)
        .map(|index| format!("field_{index} Int64"))
        .collect::<Vec<_>>()
        .join(", ");

    for (sql, expected_offset, expected_error) in [
        (
            format!("CREATE TABLE long_name ({long_field} String);"),
            "CREATE TABLE long_name (".len(),
            SchemaError::IdentifierTooLong {
                field: long_field,
                length: MAX_IDENTIFIER_BYTES + 1,
                limit: MAX_IDENTIFIER_BYTES,
            },
        ),
        (
            format!("CREATE TABLE wide ({definitions});"),
            format!("CREATE TABLE wide ({definitions}")
                .rfind(&format!("field_{}", MAX_SCHEMA_FIELDS))
                .unwrap(),
            SchemaError::TooManyFields {
                limit: MAX_SCHEMA_FIELDS,
                actual: MAX_SCHEMA_FIELDS + 1,
            },
        ),
    ] {
        let mut database = database_with_seed();
        let before = database.clone();

        let error = database.execute(&sql).unwrap_err();

        assert_eq!(error.byte_offset(), expected_offset);
        assert_eq!(
            error.kind(),
            &SqlErrorKind::InvalidSchema {
                table: if matches!(expected_error, SchemaError::IdentifierTooLong { .. }) {
                    "long_name".into()
                } else {
                    "wide".into()
                },
                error: expected_error,
            }
        );
        assert_eq!(database, before);
    }
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

fn database_with_seed() -> Database {
    let mut database = Database::new();
    database.execute("CREATE TABLE seed (id Int64);").unwrap();
    database
}
