use rusthouse::sql::execute_insert;
use rusthouse::{
    Column, ColumnSchema, ComparisonOperator, DataType, Schema, Table, Value, write_csv_with_names,
};

#[test]
fn inserts_scans_and_streams_selected_rows() {
    let mut table = Table::new(
        Schema::new(vec![
            ColumnSchema::new("id", DataType::Int64),
            ColumnSchema::new("name", DataType::String),
        ]),
        3,
    );
    execute_insert(
        "INSERT INTO events VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma')",
        "events",
        &mut table,
    )
    .unwrap();

    let selected = table
        .scan(0, ComparisonOperator::GreaterThan, &Value::Int64(1))
        .unwrap();
    let (Column::Int64(ids), Column::String(names)) = (&table.columns()[0], &table.columns()[1])
    else {
        panic!("table columns must match the schema");
    };

    let records = selected
        .into_iter()
        .map(|row| [ids[row].to_string(), names[row].clone()]);
    let mut output = Vec::new();
    write_csv_with_names(&mut output, ["id", "name"], records).unwrap();

    assert_eq!(
        output,
        b"\"id\",\"name\"\r\n\"2\",\"beta\"\r\n\"3\",\"gamma\"\r\n"
    );
}
