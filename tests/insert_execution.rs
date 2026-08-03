use rusthouse::{
    InsertError, InsertExecutionError, Int64Table, ParseLimits, Schema, execute_insert,
    parse_insert,
};

fn table(nullable: bool, row_cap: usize) -> Int64Table {
    Int64Table::new(Schema::int64("value", nullable), row_cap)
}

#[test]
fn executes_all_parsed_rows_in_one_batch() {
    let statement = parse_insert(
        "INSERT INTO readings VALUES (-9223372036854775808), (0), (9223372036854775807)",
        ParseLimits::default(),
    )
    .unwrap();
    let mut table = table(false, 3);

    execute_insert("readings", &mut table, &statement).unwrap();

    assert_eq!(table.values(), &[Some(i64::MIN), Some(0), Some(i64::MAX)]);
}

#[test]
fn executes_nulls_and_values_against_a_nullable_table() {
    let statement = parse_insert(
        "INSERT INTO readings VALUES (NULL), (7), (NULL)",
        ParseLimits::default(),
    )
    .unwrap();
    let mut table = table(true, 3);

    execute_insert("readings", &mut table, &statement).unwrap();

    assert_eq!(table.values(), &[None, Some(7), None]);
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
    let statement = parse_insert(
        "INSERT INTO readings VALUES (2), (NULL), (3)",
        ParseLimits::default(),
    )
    .unwrap();
    let mut table = table(false, 4);
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
    let statement = parse_insert(
        "INSERT INTO readings VALUES (2), (3), (4)",
        ParseLimits::default(),
    )
    .unwrap();
    let mut table = table(false, 3);
    table.append(Some(1)).unwrap();

    let error = execute_insert("readings", &mut table, &statement).unwrap_err();

    assert_eq!(
        error,
        InsertExecutionError::Insert(InsertError::RowCapExceeded {
            row_cap: 3,
            current_rows: 1,
            incoming_rows: 3,
        })
    );
    assert_eq!(table.values(), &[Some(1)]);
}

#[test]
fn accepts_a_batch_that_exactly_fills_the_row_cap() {
    let statement = parse_insert(
        "INSERT INTO readings VALUES (2), (3)",
        ParseLimits::default(),
    )
    .unwrap();
    let mut table = table(false, 3);
    table.append(Some(1)).unwrap();

    execute_insert("readings", &mut table, &statement).unwrap();

    assert_eq!(table.values(), &[Some(1), Some(2), Some(3)]);
}
