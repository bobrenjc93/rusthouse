use rusthouse::{ComparisonOperator, DataType, Field, ReductionError, RowSelection, Table, Value};

#[test]
fn empty_count_and_sum_return_typed_zeroes() {
    let table = numeric_table();
    let selection = RowSelection::try_empty(0).unwrap();

    assert_eq!(table.count(None), Ok(0));
    assert_eq!(table.count(Some(&selection)), Ok(0));
    assert_eq!(table.sum("integer", None), Ok(Value::Int64(0)));
    assert_eq!(
        table.sum("float", Some(&selection)),
        Ok(Value::Float64(0.0))
    );
    let Value::Float64(float_sum) = table.sum("float", None).unwrap() else {
        panic!("Float64 columns must produce Float64 sums");
    };
    assert_eq!(float_sum.to_bits(), 0.0_f64.to_bits());
}

#[test]
fn full_table_count_and_numeric_sums_preserve_physical_types() {
    let mut table = numeric_table();
    table
        .insert_batch([
            vec![Value::Int64(-7), Value::Float64(1.25)],
            vec![Value::Int64(12), Value::Float64(-3.5)],
            vec![Value::Int64(4), Value::Float64(0.75)],
        ])
        .unwrap();

    assert_eq!(table.count(None), Ok(3));
    assert_eq!(table.sum("integer", None), Ok(Value::Int64(9)));
    assert_eq!(table.sum("float", None), Ok(Value::Float64(-1.5)));
}

#[test]
fn count_and_sum_only_rows_set_in_a_packed_selection() {
    let mut table = numeric_table();
    table
        .insert_batch(
            (0_i64..10).map(|value| vec![Value::Int64(value), Value::Float64(value as f64 + 0.5)]),
        )
        .unwrap();
    let selection = table
        .scan(
            "integer",
            ComparisonOperator::GreaterThanOrEqual,
            &Value::Int64(7),
        )
        .unwrap();

    assert_eq!(selection.as_bytes(), &[0b1000_0000, 0b0000_0011]);
    assert_eq!(table.count(Some(&selection)), Ok(3));
    assert_eq!(table.sum("integer", Some(&selection)), Ok(Value::Int64(24)));
    assert_eq!(
        table.sum("float", Some(&selection)),
        Ok(Value::Float64(25.5))
    );
}

#[test]
fn int64_sum_accepts_exact_boundaries_and_reports_checked_overflow() {
    let mut boundaries = Table::new(vec![Field::new("value", DataType::Int64)]).unwrap();
    boundaries
        .insert_batch([vec![Value::Int64(i64::MAX)], vec![Value::Int64(i64::MIN)]])
        .unwrap();

    let maximum = boundaries
        .scan("value", ComparisonOperator::GreaterThan, &Value::Int64(0))
        .unwrap();
    let minimum = boundaries
        .scan("value", ComparisonOperator::LessThan, &Value::Int64(0))
        .unwrap();
    assert_eq!(
        boundaries.sum("value", Some(&maximum)),
        Ok(Value::Int64(i64::MAX))
    );
    assert_eq!(
        boundaries.sum("value", Some(&minimum)),
        Ok(Value::Int64(i64::MIN))
    );
    assert_eq!(boundaries.sum("value", None), Ok(Value::Int64(-1)));

    let mut positive = Table::new(vec![Field::new("value", DataType::Int64)]).unwrap();
    positive
        .insert_batch([vec![Value::Int64(i64::MAX)], vec![Value::Int64(1)]])
        .unwrap();
    assert_eq!(
        positive.sum("value", None),
        Err(ReductionError::Int64Overflow {
            field: "value".to_owned(),
            row: 1,
        })
    );

    let mut negative = Table::new(vec![Field::new("value", DataType::Int64)]).unwrap();
    negative
        .insert_batch([vec![Value::Int64(i64::MIN)], vec![Value::Int64(-1)]])
        .unwrap();
    assert_eq!(
        negative.sum("value", None),
        Err(ReductionError::Int64Overflow {
            field: "value".to_owned(),
            row: 1,
        })
    );
}

#[test]
fn selection_can_exclude_an_int64_overflowing_row() {
    let mut table = Table::new(vec![
        Field::new("value", DataType::Int64),
        Field::new("include", DataType::Bool),
    ])
    .unwrap();
    table
        .insert_batch([
            vec![Value::Int64(i64::MAX), Value::Bool(true)],
            vec![Value::Int64(1), Value::Bool(false)],
        ])
        .unwrap();
    let selection = table
        .scan("include", ComparisonOperator::Equal, &Value::Bool(true))
        .unwrap();

    assert_eq!(
        table.sum("value", Some(&selection)),
        Ok(Value::Int64(i64::MAX))
    );
}

#[test]
fn float64_sum_uses_ordered_ieee_754_addition() {
    let mut overflow = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    overflow
        .insert_batch([
            vec![Value::Float64(f64::MAX)],
            vec![Value::Float64(f64::MAX)],
        ])
        .unwrap();
    assert_eq!(
        overflow.sum("value", None),
        Ok(Value::Float64(f64::INFINITY))
    );

    let mut special = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    special
        .insert_batch([
            vec![Value::Float64(-0.0)],
            vec![Value::Float64(f64::INFINITY)],
            vec![Value::Float64(f64::NEG_INFINITY)],
        ])
        .unwrap();
    let Value::Float64(total) = special.sum("value", None).unwrap() else {
        panic!("Float64 columns must produce Float64 sums");
    };
    assert!(total.is_nan());

    let negative_zero = special
        .scan("value", ComparisonOperator::Equal, &Value::Float64(-0.0))
        .unwrap();
    let Value::Float64(zero) = special.sum("value", Some(&negative_zero)).unwrap() else {
        panic!("Float64 columns must produce Float64 sums");
    };
    assert_eq!(zero.to_bits(), 0.0_f64.to_bits());
}

#[test]
fn rejects_selections_for_a_different_row_count() {
    let mut table = numeric_table();
    table
        .insert_batch([vec![Value::Int64(1), Value::Float64(1.0)]])
        .unwrap();
    let selection = RowSelection::try_empty(2).unwrap();
    let expected = ReductionError::SelectionLengthMismatch {
        table_rows: 1,
        selection_rows: 2,
    };

    assert_eq!(table.count(Some(&selection)), Err(expected.clone()));
    assert_eq!(table.sum("integer", Some(&selection)), Err(expected));
}

#[test]
fn sum_reports_missing_and_nonnumeric_columns() {
    let table = Table::new(vec![
        Field::new("flag", DataType::Bool),
        Field::new("label", DataType::String),
    ])
    .unwrap();

    assert_eq!(
        table.sum("missing", None),
        Err(ReductionError::FieldNotFound {
            name: "missing".to_owned(),
        })
    );
    assert_eq!(
        table.sum("flag", None),
        Err(ReductionError::NonNumericColumn {
            field: "flag".to_owned(),
            data_type: DataType::Bool,
        })
    );
    assert_eq!(
        table.sum("label", None),
        Err(ReductionError::NonNumericColumn {
            field: "label".to_owned(),
            data_type: DataType::String,
        })
    );
}

fn numeric_table() -> Table {
    Table::new(vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
    ])
    .unwrap()
}
