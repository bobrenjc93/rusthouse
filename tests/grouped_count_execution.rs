use rusthouse::{
    Catalog, CatalogError, CatalogLimits, GroupedCountError, GroupedCountLimits, Int64Table,
    NullableI64GroupedCount, ParseLimits, Schema, SelectExecutionError, execute_grouped_count,
    parse_grouped_count,
};

fn table(values: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("value", true), values.len());
    table.append_batch(values).unwrap();
    table
}

fn pairs(groups: Vec<NullableI64GroupedCount>) -> Vec<(Option<i64>, u64)> {
    groups.into_iter().map(|group| group.into_pair()).collect()
}

#[test]
fn executes_an_empty_grouped_count() {
    let statement = parse_grouped_count(
        "SELECT value, COUNT(*) FROM readings GROUP BY value;",
        ParseLimits::default(),
    )
    .unwrap();

    let groups = execute_grouped_count(
        "readings",
        &table(&[]),
        &statement,
        GroupedCountLimits::new(0, 0),
    )
    .unwrap();

    assert!(groups.is_empty());
}

#[test]
fn counts_repeated_and_null_values_in_deterministic_null_first_order() {
    let statement = parse_grouped_count(
        "SELECT value, COUNT(*) FROM readings GROUP BY value",
        ParseLimits::default(),
    )
    .unwrap();
    let table = table(&[Some(7), None, Some(-2), Some(7), None, Some(-2)]);

    let groups = execute_grouped_count(
        "readings",
        &table,
        &statement,
        GroupedCountLimits::new(6, 3),
    )
    .unwrap();

    assert_eq!(pairs(groups), [(None, 2), (Some(-2), 2), (Some(7), 2)]);
}

#[test]
fn validates_table_selected_column_and_grouped_column_identifiers() {
    let table = table(&[Some(1), None]);
    let cases = [
        (
            "SELECT value, COUNT(*) FROM Readings GROUP BY value",
            "readings",
            SelectExecutionError::UnknownTable {
                name: "Readings".to_owned(),
            },
        ),
        (
            "SELECT other, COUNT(*) FROM readings GROUP BY value",
            "readings",
            SelectExecutionError::UnknownColumn {
                name: "other".to_owned(),
            },
        ),
        (
            "SELECT value, COUNT(*) FROM readings GROUP BY Other",
            "readings",
            SelectExecutionError::UnknownColumn {
                name: "Other".to_owned(),
            },
        ),
    ];

    for (input, expected_table, expected_error) in cases {
        let statement = parse_grouped_count(input, ParseLimits::default()).unwrap();
        assert_eq!(
            execute_grouped_count(
                expected_table,
                &table,
                &statement,
                GroupedCountLimits::new(2, 2),
            ),
            Err(expected_error),
            "{input:?}"
        );
    }
}

#[test]
fn preserves_explicit_input_and_distinct_group_caps() {
    let statement = parse_grouped_count(
        "SELECT value, COUNT(*) FROM readings GROUP BY value",
        ParseLimits::default(),
    )
    .unwrap();
    let table = table(&[Some(2), None, Some(1), Some(2)]);

    assert_eq!(
        execute_grouped_count(
            "readings",
            &table,
            &statement,
            GroupedCountLimits::new(3, 3),
        ),
        Err(SelectExecutionError::GroupedCount(
            GroupedCountError::InputLimitExceeded {
                rows: 4,
                max_rows: 3,
            }
        ))
    );
    assert_eq!(
        execute_grouped_count(
            "readings",
            &table,
            &statement,
            GroupedCountLimits::new(4, 2),
        ),
        Err(SelectExecutionError::GroupedCount(
            GroupedCountError::DistinctGroupLimitExceeded {
                groups: 3,
                max_groups: 2,
            }
        ))
    );
}

#[test]
fn catalog_parses_and_executes_grouped_count_sql_with_the_supplied_caps() {
    let parse_limits = ParseLimits::default();
    let mut catalog = Catalog::new(CatalogLimits::new(1, 5));
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", parse_limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO readings VALUES (7), (NULL), (7)", parse_limits)
        .unwrap();

    let groups = catalog
        .execute_grouped_count(
            "SELECT value, COUNT(*) FROM readings GROUP BY value;",
            parse_limits,
            GroupedCountLimits::new(3, 2),
        )
        .unwrap();
    assert_eq!(pairs(groups), [(None, 1), (Some(7), 2)]);

    assert_eq!(
        catalog.execute_grouped_count(
            "SELECT value, COUNT(*) FROM readings GROUP BY value",
            parse_limits,
            GroupedCountLimits::new(3, 1),
        ),
        Err(CatalogError::Select(SelectExecutionError::GroupedCount(
            GroupedCountError::DistinctGroupLimitExceeded {
                groups: 2,
                max_groups: 1,
            }
        )))
    );
}
