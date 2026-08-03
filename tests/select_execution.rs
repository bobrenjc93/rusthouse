use rusthouse::{
    Int64Table, ParseLimits, Schema, SelectExecutionError, execute_select, parse_select,
};

fn table(nullable: bool, values: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("value", nullable), values.len());
    table.append_batch(values).unwrap();
    table
}

#[test]
fn executes_a_parsed_select_against_an_empty_table() {
    let statement = parse_select("SELECT value FROM readings", ParseLimits::default()).unwrap();
    let table = table(false, &[]);

    let values = execute_select("readings", &table, &statement).unwrap();

    assert!(values.is_empty());
    assert!(std::ptr::eq(values, table.values()));
}

#[test]
fn borrows_populated_values_in_row_order() {
    let statement = parse_select("SELECT value FROM readings", ParseLimits::default()).unwrap();
    let table = table(false, &[Some(i64::MIN), Some(0), Some(i64::MAX)]);

    let values = execute_select("readings", &table, &statement).unwrap();

    assert_eq!(values, &[Some(i64::MIN), Some(0), Some(i64::MAX)]);
    assert!(std::ptr::eq(values, table.values()));
}

#[test]
fn returns_null_values_without_copying() {
    let statement = parse_select("SELECT value FROM readings", ParseLimits::default()).unwrap();
    let table = table(true, &[Some(1), None, Some(3)]);

    let values = execute_select("readings", &table, &statement).unwrap();

    assert_eq!(values, &[Some(1), None, Some(3)]);
    assert!(std::ptr::eq(values, table.values()));
}

#[test]
fn table_name_matching_is_exact_and_a_mismatch_does_not_mutate() {
    let statement = parse_select("SELECT value FROM Readings", ParseLimits::default()).unwrap();
    let table = table(true, &[Some(1), None]);
    let original = table.clone();

    let error = execute_select("readings", &table, &statement).unwrap_err();

    assert_eq!(
        error,
        SelectExecutionError::UnknownTable {
            name: "Readings".to_owned(),
        }
    );
    assert_eq!(table, original);
}

#[test]
fn column_name_matching_is_exact_and_a_mismatch_does_not_mutate() {
    let statement = parse_select("SELECT Value FROM readings", ParseLimits::default()).unwrap();
    let table = table(true, &[Some(1), None]);
    let original = table.clone();

    let error = execute_select("readings", &table, &statement).unwrap_err();

    assert_eq!(
        error,
        SelectExecutionError::UnknownColumn {
            name: "Value".to_owned(),
        }
    );
    assert_eq!(table, original);
}
