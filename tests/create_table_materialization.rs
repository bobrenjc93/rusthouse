use rusthouse::{DataType, ParseLimits, materialize_create_table, parse_create_table};

#[test]
fn parsed_nullable_create_materializes_an_empty_named_table() {
    let statement = parse_create_table(
        "CREATE TABLE EventLog (OptionalValue Int64 NULL)",
        ParseLimits::default(),
    )
    .unwrap();

    let entry = materialize_create_table(statement, 37);
    let table = entry.table();

    assert_eq!(entry.table_name().as_str(), "EventLog");
    assert!(table.is_empty());
    assert_eq!(table.row_count(), 0);
    assert_eq!(table.row_cap(), 37);
    assert_eq!(table.schema().column().name(), "OptionalValue");
    assert_eq!(table.schema().column().data_type(), DataType::Int64);
    assert!(table.schema().column().is_nullable());
}

#[test]
fn parsed_non_nullable_create_preserves_schema_spelling_and_zero_cap() {
    let statement = parse_create_table(
        "create table MiXeD_Name (ExactColumn int64 not null)",
        ParseLimits::default(),
    )
    .unwrap();

    let entry = materialize_create_table(statement, 0);
    let (table_name, table) = entry.into_parts();

    assert_eq!(table_name.as_str(), "MiXeD_Name");
    assert_eq!(table.schema().column().name(), "ExactColumn");
    assert!(!table.schema().column().is_nullable());
    assert_eq!(table.row_cap(), 0);
    assert!(table.values().is_empty());
}
