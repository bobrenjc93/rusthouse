use rusthouse::{
    Catalog, CatalogError, CatalogLimits, JoinError, JoinLimits, LeftJoinExecutionError,
    ParseLimits,
};

const JOIN_SQL: &str =
    "SELECT right_key FROM left_rows LEFT JOIN right_rows ON left_key = right_key;";

fn new_catalog(max_rows_per_table: usize) -> Catalog {
    let mut catalog = Catalog::new(CatalogLimits::new(2, max_rows_per_table));
    let parse_limits = ParseLimits::default();
    catalog
        .execute_create("CREATE TABLE left_rows (left_key Int64 NULL)", parse_limits)
        .unwrap();
    catalog
        .execute_create(
            "CREATE TABLE right_rows (right_key Int64 NULL)",
            parse_limits,
        )
        .unwrap();
    catalog
}

fn insert(catalog: &mut Catalog, table: &str, values: &[Option<i64>]) {
    for value in values {
        let literal = value.map_or_else(|| "NULL".to_owned(), |value| value.to_string());
        catalog
            .execute_insert(
                &format!("INSERT INTO {table} VALUES ({literal})"),
                ParseLimits::default(),
            )
            .unwrap();
    }
}

#[test]
fn catalog_left_join_projects_duplicates_and_typed_nulls_in_left_major_order() {
    let mut catalog = new_catalog(4);
    insert(
        &mut catalog,
        "left_rows",
        &[Some(7), None, Some(7), Some(8)],
    );
    insert(
        &mut catalog,
        "right_rows",
        &[Some(7), Some(7), None, Some(9)],
    );

    assert_eq!(
        catalog.execute_left_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(4, 6)),
        Ok(vec![Some(7), Some(7), None, Some(7), Some(7), None])
    );
}

#[test]
fn catalog_left_join_handles_all_empty_input_combinations() {
    let empty = new_catalog(2);
    assert_eq!(
        empty.execute_left_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(0, 0)),
        Ok(vec![])
    );

    let mut empty_left = new_catalog(2);
    insert(&mut empty_left, "right_rows", &[Some(1)]);
    assert_eq!(
        empty_left.execute_left_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(1, 0)),
        Ok(vec![])
    );

    let mut empty_right = new_catalog(2);
    insert(&mut empty_right, "left_rows", &[Some(1), None]);
    assert_eq!(
        empty_right.execute_left_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(2, 2)),
        Ok(vec![None, None])
    );
}

#[test]
fn catalog_left_join_reports_unknown_tables_and_each_column_role() {
    let catalog = new_catalog(0);
    let cases = [
        (
            "SELECT right_key FROM missing LEFT JOIN right_rows ON left_key = right_key",
            LeftJoinExecutionError::UnknownTable {
                name: "missing".to_owned(),
            },
        ),
        (
            "SELECT right_key FROM left_rows LEFT JOIN missing ON left_key = right_key",
            LeftJoinExecutionError::UnknownTable {
                name: "missing".to_owned(),
            },
        ),
        (
            "SELECT missing FROM left_rows LEFT JOIN right_rows ON left_key = right_key",
            LeftJoinExecutionError::UnknownColumn {
                name: "missing".to_owned(),
            },
        ),
        (
            "SELECT right_key FROM left_rows LEFT JOIN right_rows ON missing = right_key",
            LeftJoinExecutionError::UnknownColumn {
                name: "missing".to_owned(),
            },
        ),
        (
            "SELECT right_key FROM left_rows LEFT JOIN right_rows ON left_key = missing",
            LeftJoinExecutionError::UnknownColumn {
                name: "missing".to_owned(),
            },
        ),
    ];

    for (sql, expected) in cases {
        assert_eq!(
            catalog.execute_left_join(sql, ParseLimits::default(), JoinLimits::new(0, 0)),
            Err(CatalogError::LeftJoin(expected)),
            "{sql:?}"
        );
    }
}

#[test]
fn catalog_left_join_accepts_exact_caps_and_rejects_each_exceeded_cap() {
    let mut catalog = new_catalog(3);
    insert(&mut catalog, "left_rows", &[Some(1), Some(1), None]);
    insert(&mut catalog, "right_rows", &[Some(1), None, Some(1)]);

    assert_eq!(
        catalog.execute_left_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(3, 5)),
        Ok(vec![Some(1), Some(1), Some(1), Some(1), None])
    );
    assert_eq!(
        catalog.execute_left_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(3, 4)),
        Err(CatalogError::LeftJoin(LeftJoinExecutionError::Join(
            JoinError::OutputLimitExceeded {
                pairs: 5,
                max_pairs: 4,
            }
        )))
    );
    assert_eq!(
        catalog.execute_left_join(
            JOIN_SQL,
            ParseLimits::default(),
            JoinLimits::new(2, usize::MAX),
        ),
        Err(CatalogError::LeftJoin(LeftJoinExecutionError::Join(
            JoinError::LeftInputLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        )))
    );

    let mut right_exceeded = new_catalog(3);
    insert(&mut right_exceeded, "left_rows", &[Some(1), Some(1)]);
    insert(
        &mut right_exceeded,
        "right_rows",
        &[Some(1), Some(1), Some(1)],
    );
    assert_eq!(
        right_exceeded.execute_left_join(
            JOIN_SQL,
            ParseLimits::default(),
            JoinLimits::new(2, usize::MAX),
        ),
        Err(CatalogError::LeftJoin(LeftJoinExecutionError::Join(
            JoinError::RightInputLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        )))
    );
}
