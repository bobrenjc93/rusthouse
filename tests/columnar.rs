use rusthouse::{Column, DataType, NamedColumn, RecordBatch, ResultColumn, execute};

#[test]
fn storage_and_query_results_share_logical_types() {
    let batch =
        RecordBatch::try_new(vec![NamedColumn::new("answer", Column::Int64(vec![42]))]).unwrap();
    let result = execute("SELECT 42 AS answer").unwrap();
    let result_column: &ResultColumn = &result.columns[0];

    assert_eq!(batch.column("answer").unwrap().data_type(), DataType::Int64);
    assert_eq!(result_column.data_type, DataType::Int64);
}
