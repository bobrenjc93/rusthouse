use rusthouse::{Database, DatabaseError, MAX_SCRIPT_STATEMENTS, SelectResult, Value};

#[test]
fn preserves_catalog_state_across_execution_calls() {
    let mut database = Database::new();

    assert!(
        database
            .execute("CREATE TABLE events (id Int64, label String)")
            .unwrap()
            .is_empty()
    );
    assert!(
        database
            .execute("INSERT INTO events VALUES (7, 'first')")
            .unwrap()
            .is_empty()
    );

    let results = database.execute("SELECT label, id FROM events").unwrap();
    let SelectResult::Table(result) = &results[0] else {
        panic!("table projection was dispatched as a scalar SELECT");
    };
    assert_eq!(
        result.rows(),
        [vec![Value::String("first".to_owned()), Value::Int64(7)]]
    );
    assert_eq!(database.catalog().table("events").unwrap().row_count(), 1);
}

#[test]
fn returns_only_select_results_in_statement_order() {
    let mut database = Database::new();
    let results = database
        .execute(
            "CREATE TABLE values_table (value Int64);\
             INSERT INTO values_table VALUES (9);\
             SELECT ';' AS scalar;\
             /* this semicolon is not a separator: ; */\
             SELECT value FROM values_table;",
        )
        .unwrap();

    assert_eq!(results.len(), 2);
    let SelectResult::Scalar(scalar) = &results[0] else {
        panic!("first result is not scalar");
    };
    assert_eq!(scalar.value(), &Value::String(";".to_owned()));
    let SelectResult::Table(table) = &results[1] else {
        panic!("second result is not a table projection");
    };
    assert_eq!(table.rows(), [vec![Value::Int64(9)]]);
}

#[test]
fn rejects_empty_unknown_and_overlong_statement_sequences() {
    let mut database = Database::new();

    assert!(matches!(
        database.execute(" -- no statements\n"),
        Err(DatabaseError::EmptyScript)
    ));
    assert!(matches!(
        database.execute("SELECT 1;; SELECT 2"),
        Err(DatabaseError::EmptyStatement { .. })
    ));
    assert!(matches!(
        database.execute("DROP TABLE events"),
        Err(DatabaseError::UnsupportedStatement { .. })
    ));

    let at_limit = std::iter::repeat_n("SELECT 1", MAX_SCRIPT_STATEMENTS)
        .collect::<Vec<_>>()
        .join(";");
    assert_eq!(
        database.execute(&at_limit).unwrap().len(),
        MAX_SCRIPT_STATEMENTS
    );

    let oversized = std::iter::repeat_n("SELECT 1", MAX_SCRIPT_STATEMENTS + 1)
        .collect::<Vec<_>>()
        .join(";");
    assert!(matches!(
        database.execute(&oversized),
        Err(DatabaseError::StatementLimitExceeded {
            limit: MAX_SCRIPT_STATEMENTS
        })
    ));
}
