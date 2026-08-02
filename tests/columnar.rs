use rusthouse::{
    Column, DataType, NamedColumn, RecordBatch, RecordBatchError, ResultColumn, execute,
};

#[test]
fn storage_and_query_results_share_logical_types() {
    let batch =
        RecordBatch::try_new(vec![NamedColumn::new("answer", Column::Int64(vec![42]))]).unwrap();
    let result = execute("SELECT 42 AS answer").unwrap();
    let result_column: &ResultColumn = &result.columns[0];

    assert_eq!(batch.column("answer").unwrap().data_type(), DataType::Int64);
    assert_eq!(result_column.data_type, DataType::Int64);
}

fn mixed_batch() -> RecordBatch {
    RecordBatch::try_new(vec![
        NamedColumn::new("id", Column::Int64(vec![10, 20, 30, 40])),
        NamedColumn::new("score", Column::Float64(vec![1.5, 2.5, 3.5, 4.5])),
        NamedColumn::new("active", Column::Bool(vec![true, false, true, false])),
        NamedColumn::new(
            "name",
            Column::String(vec![
                "alpha".to_owned(),
                "beta".to_owned(),
                "gamma".to_owned(),
                "delta".to_owned(),
            ]),
        ),
    ])
    .unwrap()
}

#[test]
fn filters_every_column_type_and_preserves_schema_and_row_order() {
    let filtered = mixed_batch().filter(&[false, true, false, true]).unwrap();

    assert_eq!(filtered.row_count(), 2);
    assert_eq!(
        filtered,
        RecordBatch::try_new(vec![
            NamedColumn::new("id", Column::Int64(vec![20, 40])),
            NamedColumn::new("score", Column::Float64(vec![2.5, 4.5])),
            NamedColumn::new("active", Column::Bool(vec![false, false])),
            NamedColumn::new(
                "name",
                Column::String(vec!["beta".to_owned(), "delta".to_owned()]),
            ),
        ])
        .unwrap()
    );
}

#[test]
fn filtering_all_rows_preserves_the_batch() {
    let batch = mixed_batch();

    assert_eq!(batch.filter(&[true; 4]).unwrap(), batch);
}

#[test]
fn filtering_no_rows_preserves_the_schema() {
    let filtered = mixed_batch().filter(&[false; 4]).unwrap();

    assert_eq!(filtered.row_count(), 0);
    assert_eq!(filtered.column_count(), 4);
    assert_eq!(filtered.columns()[0].name(), "id");
    assert_eq!(filtered.column("id"), Some(&Column::Int64(vec![])));
    assert_eq!(filtered.column("score"), Some(&Column::Float64(vec![])));
    assert_eq!(filtered.column("active"), Some(&Column::Bool(vec![])));
    assert_eq!(filtered.column("name"), Some(&Column::String(vec![])));
}

#[test]
fn filtering_an_empty_batch_with_an_empty_mask_succeeds() {
    let batch = RecordBatch::try_new(vec![
        NamedColumn::new("id", Column::Int64(vec![])),
        NamedColumn::new("name", Column::String(vec![])),
    ])
    .unwrap();

    assert_eq!(batch.filter(&[]).unwrap(), batch);
}

#[test]
fn filtering_rejects_masks_with_the_wrong_length() {
    let batch = mixed_batch();

    assert_eq!(
        batch.filter(&[true; 3]).unwrap_err(),
        RecordBatchError::MaskLengthMismatch {
            expected: 4,
            actual: 3,
        }
    );
    assert_eq!(
        batch.filter(&[true; 5]).unwrap_err(),
        RecordBatchError::MaskLengthMismatch {
            expected: 4,
            actual: 5,
        }
    );
}
