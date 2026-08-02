use rusthouse::{ColumnDef, DataType, Error, StorageLimits, Table, Value};

fn string_table(limits: StorageLimits) -> Table {
    Table::new_with_limits(
        "messages",
        vec![ColumnDef::new("body", DataType::String)],
        limits,
    )
    .unwrap()
}

#[test]
fn enforces_schema_and_per_string_limits() {
    let limits = StorageLimits::new(1, 4, 4, 16);
    let too_wide = Table::new_with_limits(
        "wide",
        vec![
            ColumnDef::new("first", DataType::Int64),
            ColumnDef::new("second", DataType::Int64),
        ],
        limits,
    );
    assert_eq!(
        too_wide,
        Err(Error::ColumnLimitExceeded {
            limit: 1,
            actual: 2,
        })
    );

    let mut table = string_table(limits);
    assert_eq!(table.limits(), limits);
    let error = table
        .insert_row(vec![Value::String("12345".to_owned())])
        .unwrap_err();
    assert_eq!(
        error,
        Error::StringValueLimitExceeded {
            table: "messages".to_owned(),
            column: "body".to_owned(),
            limit: 4,
            actual: 5,
        }
    );
    assert!(table.is_empty());
    assert_eq!(table.value_bytes(), 0);
}

#[test]
fn accepts_exact_boundaries_and_rejects_row_or_value_growth() {
    let mut row_limited = string_table(StorageLimits::new(1, 2, 4, 4));
    row_limited
        .insert_rows(vec![
            vec![Value::String("1234".to_owned())],
            vec![Value::String(String::new())],
        ])
        .unwrap();
    assert_eq!(row_limited.row_count(), 2);
    assert_eq!(row_limited.value_bytes(), 4);

    let original = row_limited.clone();
    assert_eq!(
        row_limited.insert_row(vec![Value::String(String::new())]),
        Err(Error::RowLimitExceeded {
            table: "messages".to_owned(),
            limit: 2,
            actual: 3,
        })
    );
    assert_eq!(row_limited, original);

    let mut byte_limited = string_table(StorageLimits::new(1, 3, 4, 5));
    byte_limited
        .insert_row(vec![Value::String("1234".to_owned())])
        .unwrap();
    let original = byte_limited.clone();
    assert_eq!(
        byte_limited.insert_row(vec![Value::String("12".to_owned())]),
        Err(Error::ValueStorageLimitExceeded {
            table: "messages".to_owned(),
            limit: 5,
            actual: 6,
        })
    );
    assert_eq!(byte_limited, original);
}

#[test]
fn batch_resource_failures_are_transactional() {
    let mut table = string_table(StorageLimits::new(1, 2, 4, 5));
    table
        .insert_row(vec![Value::String("a".to_owned())])
        .unwrap();
    let original = table.clone();

    assert_eq!(
        table.insert_rows(vec![
            vec![Value::String("bb".to_owned())],
            vec![Value::String("cc".to_owned())],
        ]),
        Err(Error::RowLimitExceeded {
            table: "messages".to_owned(),
            limit: 2,
            actual: 3,
        })
    );
    assert_eq!(table, original);

    let mut table = string_table(StorageLimits::new(1, 4, 4, 5));
    table
        .insert_row(vec![Value::String("a".to_owned())])
        .unwrap();
    let original = table.clone();
    assert_eq!(
        table.insert_rows(vec![
            vec![Value::String("bb".to_owned())],
            vec![Value::String("ccc".to_owned())],
        ]),
        Err(Error::ValueStorageLimitExceeded {
            table: "messages".to_owned(),
            limit: 5,
            actual: 6,
        })
    );
    assert_eq!(table, original);
}
