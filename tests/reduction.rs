use rusthouse::{ComparisonOperator, DataType, Field, ReductionError, RowSelection, Table, Value};

#[test]
fn min_and_max_preserve_every_physical_type_for_full_inputs() {
    let mut table = all_types_table();
    table
        .insert_batch([
            row(7, 3.5, true, "pear"),
            row(-4, -8.25, false, "apple"),
            row(12, 1.0, true, "orange"),
        ])
        .unwrap();

    assert_extrema(&table, "integer", None, Value::Int64(-4), Value::Int64(12));
    assert_extrema(
        &table,
        "float",
        None,
        Value::Float64(-8.25),
        Value::Float64(3.5),
    );
    assert_extrema(
        &table,
        "boolean",
        None,
        Value::Bool(false),
        Value::Bool(true),
    );
    assert_extrema(
        &table,
        "text",
        None,
        Value::String("apple".to_owned()),
        Value::String("pear".to_owned()),
    );
}

#[test]
fn min_and_max_only_consider_selected_rows_for_every_type() {
    let mut table = Table::new(vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
        Field::new("include", DataType::Bool),
    ])
    .unwrap();
    table
        .insert_batch([
            filtered_row(-100, -100.0, false, "aardvark", false),
            filtered_row(8, 4.0, false, "lime", true),
            filtered_row(2, 9.0, true, "plum", true),
            filtered_row(100, 100.0, true, "zebra", false),
        ])
        .unwrap();
    let selection = table
        .scan("include", ComparisonOperator::Equal, &Value::Bool(true))
        .unwrap();

    assert_eq!(selection.selected_rows().collect::<Vec<_>>(), [1, 2]);
    assert_extrema(
        &table,
        "integer",
        Some(&selection),
        Value::Int64(2),
        Value::Int64(8),
    );
    assert_extrema(
        &table,
        "float",
        Some(&selection),
        Value::Float64(4.0),
        Value::Float64(9.0),
    );
    assert_extrema(
        &table,
        "boolean",
        Some(&selection),
        Value::Bool(false),
        Value::Bool(true),
    );
    assert_extrema(
        &table,
        "text",
        Some(&selection),
        Value::String("lime".to_owned()),
        Value::String("plum".to_owned()),
    );
}

#[test]
fn min_and_max_return_none_for_empty_tables_and_empty_selections() {
    let empty = all_types_table();
    let empty_table_selection = RowSelection::try_empty(0).unwrap();

    for field in ["integer", "float", "boolean", "text"] {
        assert_eq!(empty.min(field, None), Ok(None));
        assert_eq!(empty.max(field, Some(&empty_table_selection)), Ok(None));
    }

    let mut populated = all_types_table();
    populated
        .insert_batch([row(1, 1.0, true, "present")])
        .unwrap();
    let empty_selection = RowSelection::try_empty(1).unwrap();
    for field in ["integer", "float", "boolean", "text"] {
        assert_eq!(populated.min(field, Some(&empty_selection)), Ok(None));
        assert_eq!(populated.max(field, Some(&empty_selection)), Ok(None));
    }
}

#[test]
fn float_extrema_use_total_nan_and_signed_zero_ordering() {
    let negative_nan = f64::from_bits(0xfff8_0000_0000_0001);
    let positive_nan = f64::from_bits(0x7ff8_0000_0000_0001);
    let mut table = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    table
        .insert_batch([
            vec![Value::Float64(0.0)],
            vec![Value::Float64(-0.0)],
            vec![Value::Float64(negative_nan)],
            vec![Value::Float64(positive_nan)],
        ])
        .unwrap();

    assert_float_bits(table.min("value", None).unwrap(), negative_nan.to_bits());
    assert_float_bits(table.max("value", None).unwrap(), positive_nan.to_bits());

    let zeros = table
        .scan("value", ComparisonOperator::Equal, &Value::Float64(0.0))
        .unwrap();
    assert_float_bits(
        table.min("value", Some(&zeros)).unwrap(),
        (-0.0_f64).to_bits(),
    );
    assert_float_bits(table.max("value", Some(&zeros)).unwrap(), 0.0_f64.to_bits());
}

#[test]
fn min_and_max_reject_mismatched_selections_before_reducing() {
    let mut table = all_types_table();
    table.insert_batch([row(1, 1.0, true, "one")]).unwrap();
    let selection = RowSelection::try_empty(2).unwrap();
    let expected = ReductionError::SelectionLengthMismatch {
        table_rows: 1,
        selection_rows: 2,
    };

    assert_eq!(
        table.min("integer", Some(&selection)),
        Err(expected.clone())
    );
    assert_eq!(table.max("text", Some(&selection)), Err(expected));
}

#[test]
fn min_and_max_report_missing_fields() {
    let table = all_types_table();
    let expected = ReductionError::FieldNotFound {
        name: "missing".to_owned(),
    };

    assert_eq!(table.min("missing", None), Err(expected.clone()));
    assert_eq!(table.max("missing", None), Err(expected));
}

fn all_types_table() -> Table {
    Table::new(vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
    ])
    .unwrap()
}

fn row(integer: i64, float: f64, boolean: bool, text: &str) -> Vec<Value> {
    vec![
        Value::Int64(integer),
        Value::Float64(float),
        Value::Bool(boolean),
        Value::String(text.to_owned()),
    ]
}

fn filtered_row(integer: i64, float: f64, boolean: bool, text: &str, include: bool) -> Vec<Value> {
    let mut row = row(integer, float, boolean, text);
    row.push(Value::Bool(include));
    row
}

fn assert_extrema(
    table: &Table,
    field: &str,
    selection: Option<&RowSelection>,
    minimum: Value,
    maximum: Value,
) {
    assert_eq!(table.min(field, selection), Ok(Some(minimum)));
    assert_eq!(table.max(field, selection), Ok(Some(maximum)));
}

fn assert_float_bits(value: Option<Value>, expected: u64) {
    let Some(Value::Float64(value)) = value else {
        panic!("Float64 extrema must produce a Float64 value");
    };
    assert_eq!(value.to_bits(), expected);
}
