use rusthouse::{
    BatchAppendError, Column, DEFAULT_TABLE_ROW_LIMIT, DataType, Database, MAX_IDENTIFIER_BYTES,
    MAX_SCHEMA_FIELDS, MAX_SQL_INPUT_BYTES, MAX_SQL_STATEMENTS, MAX_STORED_STRING_BYTES,
    ScalarValue, SchemaError, SqlErrorKind, ValueType, write_csv,
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
fn counts_rows_from_committed_and_staged_tables_case_insensitively() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE Events (id Int64); INSERT INTO events VALUES (1);")
        .unwrap();

    let results = database
        .execute(
            "SELECT COUNT(*) AS committed FROM eVeNtS;\n\
             INSERT INTO EVENTS VALUES (2), (3);\n\
             SELECT count(*) AS staged_rows FROM events;\n\
             CREATE TABLE Empty_Table (value String);\n\
             SELECT COUNT(*) FROM EMPTY_TABLE;",
        )
        .unwrap();

    assert_eq!(
        results,
        vec![
            rusthouse::QueryResult {
                header: "committed".into(),
                value: ScalarValue::Integer(1),
            },
            rusthouse::QueryResult {
                header: "staged_rows".into(),
                value: ScalarValue::Integer(3),
            },
            rusthouse::QueryResult {
                header: "COUNT(*)".into(),
                value: ScalarValue::Integer(0),
            },
        ]
    );
    assert_eq!(database.table("events").unwrap().row_count(), 3);
    assert_eq!(database.table("empty_table").unwrap().row_count(), 0);
}

#[test]
fn unknown_count_table_is_positioned_and_rolls_back_the_batch() {
    let mut database = database_with_seed();
    database.execute("INSERT INTO seed VALUES (1);").unwrap();
    let before = database.clone();
    let sql = "INSERT INTO seed VALUES (2);\n\
               CREATE TABLE staged (id Int64);\n\
               SELECT COUNT(*) AS total FROM Missing_Table;";

    let error = database.execute(sql).unwrap_err();

    assert_eq!(
        error.kind(),
        &SqlErrorKind::UnknownTable {
            table: "Missing_Table".into(),
        }
    );
    assert_eq!(error.byte_offset(), sql.find("Missing_Table").unwrap());
    assert_eq!((error.line(), error.column()), (3, 31));
    assert_eq!(database, before);
    assert!(database.table("staged").is_none());
}

#[test]
fn rejects_unsupported_count_forms_without_mutating_the_catalog() {
    for sql in [
        "SELECT COUNT() FROM seed;",
        "SELECT COUNT(id) FROM seed;",
        "SELECT COUNT(*) seed;",
        "SELECT COUNT(*) FROM;",
        "SELECT COUNT(*) FROM seed",
        "SELECT COUNT(*) FROM seed WHERE id = 1;",
        "SELECT SUM(*) FROM seed;",
        "SELECT COUNT(*), 1 FROM seed;",
        "SELECT 1, COUNT(*) FROM seed;",
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
        let expected_source = expected_error.to_string();

        let error = database.execute(&sql).unwrap_err();

        assert_eq!(error.byte_offset(), expected_offset);
        assert_eq!(
            std::error::Error::source(&error).map(ToString::to_string),
            Some(expected_source)
        );
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
fn sql_schema_and_insert_width_accept_the_boundary_and_reject_one_over() {
    let definitions = (0..MAX_SCHEMA_FIELDS)
        .map(|index| format!("field_{index} Int64"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = std::iter::repeat_n("1", MAX_SCHEMA_FIELDS)
        .collect::<Vec<_>>()
        .join(", ");
    let mut database = Database::new();

    database
        .execute(&format!(
            "CREATE TABLE wide ({definitions}); INSERT INTO wide VALUES ({values});"
        ))
        .unwrap();

    let table = database.table("wide").unwrap();
    assert_eq!(table.schema().len(), MAX_SCHEMA_FIELDS);
    assert_eq!(table.row_count(), 1);

    let sql = format!("INSERT INTO wide VALUES ({values}, 1);");
    let error = database.execute(&sql).unwrap_err();
    assert_eq!(
        error.kind(),
        &SqlErrorKind::InvalidRow {
            table: "wide".into(),
            source: BatchAppendError::RowShapeMismatch {
                row_index: 0,
                expected: MAX_SCHEMA_FIELDS,
                actual: MAX_SCHEMA_FIELDS + 1,
            },
        }
    );
    assert_eq!(database.table("wide").unwrap().row_count(), 1);
}

#[test]
fn sql_insert_rows_accept_the_boundary_and_stop_one_over() {
    const PREFIX: &str = "CREATE TABLE bounded (value Int64); INSERT INTO bounded VALUES ";
    let mut accepted_sql = String::with_capacity(PREFIX.len() + DEFAULT_TABLE_ROW_LIMIT * 4);
    accepted_sql.push_str(PREFIX);
    accepted_sql.push_str(&"(1),".repeat(DEFAULT_TABLE_ROW_LIMIT - 1));
    accepted_sql.push_str("(1);");
    let mut accepted_database = Database::new();

    accepted_database.execute(&accepted_sql).unwrap();

    assert_eq!(
        accepted_database.table("bounded").unwrap().row_count(),
        DEFAULT_TABLE_ROW_LIMIT
    );
    drop(accepted_database);
    drop(accepted_sql);

    let mut rejected_sql = String::with_capacity(PREFIX.len() + (DEFAULT_TABLE_ROW_LIMIT + 1) * 4);
    rejected_sql.push_str(PREFIX);
    rejected_sql.push_str(&"(1),".repeat(DEFAULT_TABLE_ROW_LIMIT));
    rejected_sql.push_str("(1);");
    let mut rejected_database = Database::new();

    let error = rejected_database.execute(&rejected_sql).unwrap_err();

    assert_eq!(
        error.kind(),
        &SqlErrorKind::InvalidRow {
            table: "bounded".into(),
            source: BatchAppendError::RowLimitExceeded {
                row_index: DEFAULT_TABLE_ROW_LIMIT,
                limit: DEFAULT_TABLE_ROW_LIMIT,
            },
        }
    );
    assert!(rejected_database.is_empty());
}

#[test]
fn incremental_insert_does_not_require_copying_existing_rows() {
    const EXISTING_ROWS: usize = 100_000;
    const PREFIX: &str = "CREATE TABLE events (id Int64); INSERT INTO events VALUES ";
    let mut sql = String::with_capacity(PREFIX.len() + EXISTING_ROWS * 4);
    sql.push_str(PREFIX);
    sql.push_str(&"(1),".repeat(EXISTING_ROWS - 1));
    sql.push_str("(1);");
    let mut database = Database::new();
    database.execute(&sql).unwrap();

    let error = database
        .execute("INSERT INTO events VALUES ('wrong');")
        .unwrap_err();

    assert!(matches!(
        error.kind(),
        SqlErrorKind::InvalidRow {
            source: BatchAppendError::TypeMismatch { .. },
            ..
        }
    ));
    assert_eq!(database.table("events").unwrap().row_count(), EXISTING_ROWS);

    database.execute("INSERT INTO events VALUES (2);").unwrap();
    let table = database.table("events").unwrap();
    assert_eq!(table.row_count(), EXISTING_ROWS + 1);
    let Column::Int64(ids) = table.column("id").unwrap() else {
        panic!("id should be an Int64 column");
    };
    assert_eq!(ids.values().last(), Some(&2));
}

#[test]
fn schema_limit_failure_rolls_back_an_earlier_staged_insert() {
    let mut database = database_with_seed();
    let before = database.clone();
    let long_field = "a".repeat(MAX_IDENTIFIER_BYTES + 1);
    let sql = format!("INSERT INTO seed VALUES (1); CREATE TABLE invalid ({long_field} String);");

    let error = database.execute(&sql).unwrap_err();

    assert!(matches!(
        error.kind(),
        SqlErrorKind::InvalidSchema {
            error: SchemaError::IdentifierTooLong { .. },
            ..
        }
    ));
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

#[test]
fn bounds_dense_statement_batches_and_preserves_catalog() {
    const STATEMENT: &str = "SELECT 1;";

    let mut accepted_database = Database::new();
    let accepted_sql = STATEMENT.repeat(MAX_SQL_STATEMENTS);
    let results = accepted_database.execute(&accepted_sql).unwrap();
    assert_eq!(results.len(), MAX_SQL_STATEMENTS);

    let mut rejected_database = database_with_seed();
    let before = rejected_database.clone();
    let rejected_sql = format!("{accepted_sql}{STATEMENT}");
    let error = rejected_database.execute(&rejected_sql).unwrap_err();

    assert_eq!(error.byte_offset(), accepted_sql.len());
    assert_eq!(
        error.kind(),
        &SqlErrorKind::TooManyStatements {
            max_statements: MAX_SQL_STATEMENTS,
        }
    );
    assert_eq!(rejected_database, before);
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
