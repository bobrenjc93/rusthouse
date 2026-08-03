use rusthouse::{DataType, InsertError, Int64Table, Schema};

#[test]
fn starts_empty_with_a_one_column_schema() {
    let table = Int64Table::new(Schema::int64("event_id", true), 4);

    assert!(table.is_empty());
    assert_eq!(table.row_count(), 0);
    assert_eq!(table.row_cap(), 4);
    assert_eq!(table.schema().columns().len(), 1);
    assert_eq!(table.schema().column().name(), "event_id");
    assert_eq!(table.schema().column().data_type(), DataType::Int64);
    assert!(table.schema().column().is_nullable());
    assert!(table.values().is_empty());
}

#[test]
fn nullable_column_stores_null() {
    let mut table = Int64Table::new(Schema::int64("value", true), 2);

    table.append(None).unwrap();

    assert_eq!(table.values(), &[None]);
}

#[test]
fn non_nullable_column_rejects_null_without_mutation() {
    let mut table = Int64Table::new(Schema::int64("value", false), 2);
    table.append(Some(10)).unwrap();

    let error = table.append(None).unwrap_err();

    assert_eq!(
        error,
        InsertError::NullNotAllowed {
            column: "value".to_owned(),
        }
    );
    assert_eq!(table.values(), &[Some(10)]);
}

#[test]
fn row_cap_overflow_is_typed_and_non_mutating() {
    let mut table = Int64Table::new(Schema::int64("value", true), 2);
    table.append_batch(&[Some(1), None]).unwrap();

    let error = table.append(Some(3)).unwrap_err();

    assert_eq!(
        error,
        InsertError::RowCapExceeded {
            row_cap: 2,
            current_rows: 2,
            incoming_rows: 1,
        }
    );
    assert_eq!(table.values(), &[Some(1), None]);
}

#[test]
fn batch_append_is_atomic_when_a_later_value_is_invalid() {
    let mut table = Int64Table::new(Schema::int64("value", false), 4);
    table.append(Some(1)).unwrap();

    let error = table.append_batch(&[Some(2), None, Some(3)]).unwrap_err();

    assert!(matches!(error, InsertError::NullNotAllowed { .. }));
    assert_eq!(table.into_values(), vec![Some(1)]);
}

#[test]
fn appends_values_in_row_order() {
    let mut table = Int64Table::new(Schema::int64("value", false), 4);

    table.append(Some(i64::MIN)).unwrap();
    table.append_batch(&[Some(0), Some(i64::MAX)]).unwrap();

    assert_eq!(table.values(), &[Some(i64::MIN), Some(0), Some(i64::MAX)]);
}
