use rusthouse::{
    InsertError, InsertExecutionError, Int64Table, ParseLimits, Schema, execute_insert,
    parse_insert,
};

fn table(nullable: bool, row_cap: usize) -> Int64Table {
    Int64Table::new(Schema::int64("value", nullable), row_cap)
}

#[test]
fn executes_a_parsed_insert_against_the_named_table() {
    let statement =
        parse_insert("INSERT INTO readings VALUES (-7)", ParseLimits::default()).unwrap();
    let mut table = table(false, 2);

    execute_insert("readings", &mut table, &statement).unwrap();

    assert_eq!(table.values(), &[Some(-7)]);
}

#[test]
fn executes_a_parsed_null_against_a_nullable_table() {
    let statement =
        parse_insert("INSERT INTO readings VALUES (NULL)", ParseLimits::default()).unwrap();
    let mut table = table(true, 1);

    execute_insert("readings", &mut table, &statement).unwrap();

    assert_eq!(table.values(), &[None]);
}

#[test]
fn table_name_matching_is_exact_and_a_mismatch_does_not_mutate() {
    let statement =
        parse_insert("INSERT INTO Readings VALUES (2)", ParseLimits::default()).unwrap();
    let mut table = table(false, 2);
    table.append(Some(1)).unwrap();

    let error = execute_insert("readings", &mut table, &statement).unwrap_err();

    assert_eq!(
        error,
        InsertExecutionError::UnknownTable {
            name: "Readings".to_owned(),
        }
    );
    assert_eq!(table.values(), &[Some(1)]);
}

#[test]
fn wraps_nullability_rejection_without_mutation() {
    let statement =
        parse_insert("INSERT INTO readings VALUES (NULL)", ParseLimits::default()).unwrap();
    let mut table = table(false, 2);
    table.append(Some(1)).unwrap();

    let error = execute_insert("readings", &mut table, &statement).unwrap_err();

    assert_eq!(
        error,
        InsertExecutionError::Insert(InsertError::NullNotAllowed {
            column: "value".to_owned(),
        })
    );
    assert_eq!(table.values(), &[Some(1)]);
}

#[test]
fn wraps_row_cap_rejection_without_mutation() {
    let statement =
        parse_insert("INSERT INTO readings VALUES (2)", ParseLimits::default()).unwrap();
    let mut table = table(false, 1);
    table.append(Some(1)).unwrap();

    let error = execute_insert("readings", &mut table, &statement).unwrap_err();

    assert_eq!(
        error,
        InsertExecutionError::Insert(InsertError::RowCapExceeded {
            row_cap: 1,
            current_rows: 1,
            incoming_rows: 1,
        })
    );
    assert_eq!(table.values(), &[Some(1)]);
}
