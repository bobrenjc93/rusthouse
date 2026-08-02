use rusthouse::{DataType, Database, Error, InsertError, MAX_BATCH_ROWS};

#[test]
fn inserts_multiple_rows_with_every_supported_literal_type() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE Events (id Int64, score Float64, active Bool, label String)")
        .expect("valid schema");

    database
        .execute(
            "INSERT INTO events VALUES
                (-1, +2.5, TRUE, 'O''Brien'),
                (+3, -4e-2, false, ''),
                (-9223372036854775808, .5, TrUe, 'quoted ''value''')",
        )
        .expect("valid multi-row INSERT");

    let table = database.table("EVENTS").expect("resolved target table");
    assert_eq!(table.row_count(), 3);
    assert_eq!(
        table.column_by_name("id").expect("id").as_int64(),
        Some([-1, 3, i64::MIN].as_slice())
    );
    assert_eq!(
        table.column_by_name("score").expect("score").as_float64(),
        Some([2.5, -0.04, 0.5].as_slice())
    );
    assert_eq!(
        table.column_by_name("active").expect("active").as_bool(),
        Some([true, false, true].as_slice())
    );
    assert_eq!(
        table.column_by_name("label").expect("label").as_string(),
        Some(
            [
                "O'Brien".to_owned(),
                String::new(),
                "quoted 'value'".to_owned(),
            ]
            .as_slice()
        )
    );
}

#[test]
fn a_late_row_width_error_leaves_the_sql_table_unchanged() {
    let mut database = populated_database();
    let before = database.table("events").expect("table").clone();

    assert_eq!(
        database.execute(
            "INSERT INTO events VALUES
                (2, 2.5, false, 'valid'),
                (3, 3.5, true)"
        ),
        Err(Error::Insert(InsertError::WrongRowWidth {
            row: 1,
            expected: 4,
            actual: 3,
        }))
    );
    assert_eq!(database.table("events"), Some(&before));
}

#[test]
fn a_late_type_error_leaves_the_sql_table_unchanged() {
    let mut database = populated_database();
    let before = database.table("events").expect("table").clone();

    assert_eq!(
        database.execute(
            "INSERT INTO EVENTS VALUES
                (2, 2.5, false, 'valid'),
                (3, 3.5, 'not a bool', 'invalid')"
        ),
        Err(Error::Insert(InsertError::TypeMismatch {
            row: 1,
            column: 2,
            column_name: "active".to_owned(),
            expected: DataType::Bool,
            actual: DataType::String,
        }))
    );
    assert_eq!(database.table("events"), Some(&before));
}

#[test]
fn a_late_non_finite_float_leaves_the_sql_table_unchanged() {
    let mut database = populated_database();
    let before = database.table("events").expect("table").clone();

    assert_eq!(
        database.execute(
            "INSERT INTO events VALUES
                (2, 2.5, false, 'valid'),
                (3, 1e999, true, 'invalid')"
        ),
        Err(Error::Insert(InsertError::NonFiniteFloat {
            row: 1,
            column: 1,
            column_name: "score".to_owned(),
        }))
    );
    assert_eq!(database.table("events"), Some(&before));
}

#[test]
fn rejects_inserts_into_unknown_tables_without_changing_the_catalog() {
    let mut database = Database::new();

    assert_eq!(
        database.execute("INSERT INTO missing VALUES (1)"),
        Err(Error::TableNotFound {
            name: "missing".to_owned(),
        })
    );
    assert!(database.catalog().is_empty());
}

#[test]
fn rejects_an_oversized_sql_batch_before_schema_validation_or_mutation() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64)")
        .expect("valid schema");
    let tuples = std::iter::repeat("()")
        .take(MAX_BATCH_ROWS + 1)
        .collect::<Vec<_>>()
        .join(",");
    let statement = format!("INSERT INTO events VALUES {tuples}");

    assert_eq!(
        database.execute(&statement),
        Err(Error::Insert(InsertError::BatchTooLarge {
            actual: MAX_BATCH_ROWS + 1,
            maximum: MAX_BATCH_ROWS,
        }))
    );
    assert!(database.table("events").expect("table").is_empty());
}

fn populated_database() -> Database {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, score Float64, active Bool, label String)")
        .expect("valid schema");
    database
        .execute("INSERT INTO events VALUES (1, 1.5, true, 'existing')")
        .expect("initial row");
    database
}
