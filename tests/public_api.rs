use rusthouse::sql::{Select, Statement, parse};
use rusthouse::storage::{Column, ColumnDef, Table};
use rusthouse::{DataType, Value};

#[test]
fn legacy_public_value_statement_and_column_shapes_remain_usable() {
    assert_eq!(Value::Int64(42).data_type(), DataType::Int64);

    let columns = [
        Column::Int64(vec![42]),
        Column::Float64(vec![2.5]),
        Column::Bool(vec![true]),
        Column::String(vec!["ok".to_owned()]),
    ];
    assert_eq!(columns[0].value(0), Value::Int64(42));
    assert_eq!(columns[1].value(0), Value::Float64(2.5));
    assert_eq!(columns[2].value(0), Value::Bool(true));
    assert_eq!(columns[3].value(0), Value::String("ok".to_owned()));

    let Statement::Select(select) = parse("SELECT id FROM events")
        .expect("valid SELECT")
        .remove(0)
    else {
        panic!("expected SELECT");
    };
    let _: Select = select;
}

#[test]
fn nullable_column_validity_cannot_diverge_from_values() {
    let mut table = Table::new(
        "events".to_owned(),
        vec![ColumnDef {
            name: "id".to_owned(),
            data_type: DataType::Int64,
        }],
    )
    .expect("valid table");

    table.insert_row(vec![Value::Int64(1)]).expect("row");
    table.insert_row(vec![Value::Null]).expect("NULL row");
    table.insert_row(vec![Value::Int64(3)]).expect("row");

    let column = &table.columns()[0];
    assert_eq!(column.len(), 3);
    assert_eq!(column.value(0), Value::Int64(1));
    assert_eq!(column.value(1), Value::Null);
    assert_eq!(column.value(2), Value::Int64(3));
}
