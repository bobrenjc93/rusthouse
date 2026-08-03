use rusthouse::{
    DistinctError, DistinctLimits, Int64Table, ParseLimits, Schema, SelectDistinctExecutionError,
    execute_select_distinct, parse_select_distinct,
};

fn table(values: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("value", true), values.len());
    table.append_batch(values).unwrap();
    table
}

#[test]
fn removes_duplicates_with_deterministic_null_first_output() {
    let statement = parse_select_distinct(
        "SELECT DISTINCT value FROM readings;",
        ParseLimits::default(),
    )
    .unwrap();
    let table = table(&[Some(i64::MAX), Some(7), None, Some(i64::MIN), Some(7), None]);

    assert_eq!(
        execute_select_distinct("readings", &table, &statement, DistinctLimits::new(6, 4),),
        Ok(vec![None, Some(i64::MIN), Some(7), Some(i64::MAX)])
    );
}

#[test]
fn executes_against_an_empty_table_at_zero_caps() {
    let statement = parse_select_distinct(
        "SELECT DISTINCT value FROM readings",
        ParseLimits::default(),
    )
    .unwrap();

    assert_eq!(
        execute_select_distinct(
            "readings",
            &table(&[]),
            &statement,
            DistinctLimits::new(0, 0),
        ),
        Ok(vec![])
    );
}

#[test]
fn reports_exact_table_and_column_identifier_errors() {
    let table = table(&[Some(1), None]);
    let wrong_table = parse_select_distinct(
        "SELECT DISTINCT value FROM Readings",
        ParseLimits::default(),
    )
    .unwrap();
    let wrong_column = parse_select_distinct(
        "SELECT DISTINCT Value FROM readings",
        ParseLimits::default(),
    )
    .unwrap();

    assert_eq!(
        execute_select_distinct("readings", &table, &wrong_table, DistinctLimits::new(2, 2),),
        Err(SelectDistinctExecutionError::UnknownTable {
            name: "Readings".to_owned(),
        })
    );
    assert_eq!(
        execute_select_distinct("readings", &table, &wrong_column, DistinctLimits::new(2, 2),),
        Err(SelectDistinctExecutionError::UnknownColumn {
            name: "Value".to_owned(),
        })
    );
}

#[test]
fn enforces_exact_and_exceeded_input_and_result_caps() {
    let statement = parse_select_distinct(
        "SELECT DISTINCT value FROM readings",
        ParseLimits::default(),
    )
    .unwrap();
    let table = table(&[Some(2), None, Some(1), Some(2)]);

    assert_eq!(
        execute_select_distinct("readings", &table, &statement, DistinctLimits::new(4, 3),),
        Ok(vec![None, Some(1), Some(2)])
    );
    assert_eq!(
        execute_select_distinct("readings", &table, &statement, DistinctLimits::new(3, 3),),
        Err(SelectDistinctExecutionError::Distinct(
            DistinctError::InputLimitExceeded {
                rows: 4,
                max_rows: 3,
            }
        ))
    );
    assert_eq!(
        execute_select_distinct("readings", &table, &statement, DistinctLimits::new(4, 2),),
        Err(SelectDistinctExecutionError::Distinct(
            DistinctError::DistinctValueLimitExceeded {
                values: 3,
                max_values: 2,
            }
        ))
    );
}
