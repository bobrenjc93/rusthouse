use rusthouse::storage::{ColumnDef, Table};
use rusthouse::{DataType, Value};

#[test]
fn public_table_value_reads_stored_nulls_logically() {
    let mut table = Table::new(
        "nullable_values".to_owned(),
        vec![ColumnDef {
            name: "value".to_owned(),
            data_type: DataType::Int64,
            nullable: true,
        }],
    )
    .expect("valid table");
    table
        .insert_row(vec![Value::Null])
        .expect("NULL is valid for nullable column");
    table
        .insert_row(vec![Value::Int64(7)])
        .expect("typed value is valid");

    assert_eq!(table.value(0, 0), Value::Null);
    assert_eq!(table.value(0, 1), Value::Int64(7));
}
