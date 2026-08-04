use rusthouse::{
    Catalog, CatalogError, CatalogLimits, InnerJoinExecutionError, JoinError, JoinLimits,
    ParseLimits,
};

const JOIN_SQL: &str =
    "SELECT left_key FROM left_rows INNER JOIN right_rows ON left_key = right_key;";

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
fn sql_join_preserves_cross_products_null_semantics_and_source_order() {
    let mut catalog = new_catalog(4);
    insert(
        &mut catalog,
        "left_rows",
        &[Some(7), None, Some(7), Some(8)],
    );
    insert(
        &mut catalog,
        "right_rows",
        &[Some(7), Some(7), None, Some(8)],
    );

    assert_eq!(
        catalog.execute_inner_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(4, 5),),
        Ok(vec![Some(7), Some(7), Some(7), Some(7), Some(8)])
    );
}

#[test]
fn sql_join_accepts_empty_inputs() {
    let empty = new_catalog(1);
    assert_eq!(
        empty.execute_inner_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(0, 0),),
        Ok(vec![])
    );

    let mut empty_left = new_catalog(1);
    insert(&mut empty_left, "right_rows", &[Some(1)]);
    assert_eq!(
        empty_left.execute_inner_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(1, 0),),
        Ok(vec![])
    );

    let mut empty_right = new_catalog(1);
    insert(&mut empty_right, "left_rows", &[Some(1)]);
    assert_eq!(
        empty_right.execute_inner_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(1, 0),),
        Ok(vec![])
    );
}

#[test]
fn sql_join_reports_unknown_tables_and_each_column_role() {
    let catalog = new_catalog(0);
    let cases = [
        (
            "SELECT left_key FROM missing INNER JOIN right_rows ON left_key = right_key",
            InnerJoinExecutionError::UnknownTable {
                name: "missing".to_owned(),
            },
        ),
        (
            "SELECT left_key FROM left_rows INNER JOIN missing ON left_key = right_key",
            InnerJoinExecutionError::UnknownTable {
                name: "missing".to_owned(),
            },
        ),
        (
            "SELECT missing FROM left_rows INNER JOIN right_rows ON left_key = right_key",
            InnerJoinExecutionError::UnknownColumn {
                name: "missing".to_owned(),
            },
        ),
        (
            "SELECT left_key FROM left_rows INNER JOIN right_rows ON missing = right_key",
            InnerJoinExecutionError::UnknownColumn {
                name: "missing".to_owned(),
            },
        ),
        (
            "SELECT left_key FROM left_rows INNER JOIN right_rows ON left_key = missing",
            InnerJoinExecutionError::UnknownColumn {
                name: "missing".to_owned(),
            },
        ),
    ];

    for (sql, expected) in cases {
        assert_eq!(
            catalog.execute_inner_join(sql, ParseLimits::default(), JoinLimits::new(0, 0),),
            Err(CatalogError::InnerJoin(expected)),
            "{sql:?}"
        );
    }
}

#[test]
fn sql_join_accepts_exact_caps_and_rejects_each_exceeded_cap() {
    let mut catalog = new_catalog(3);
    insert(&mut catalog, "left_rows", &[Some(1), Some(1), None]);
    insert(&mut catalog, "right_rows", &[Some(1), None, Some(1)]);

    assert_eq!(
        catalog.execute_inner_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(3, 4),),
        Ok(vec![Some(1), Some(1), Some(1), Some(1)])
    );
    assert_eq!(
        catalog.execute_inner_join(JOIN_SQL, ParseLimits::default(), JoinLimits::new(3, 3),),
        Err(CatalogError::InnerJoin(InnerJoinExecutionError::Join(
            JoinError::OutputLimitExceeded {
                pairs: 4,
                max_pairs: 3,
            }
        )))
    );
    assert_eq!(
        catalog.execute_inner_join(
            JOIN_SQL,
            ParseLimits::default(),
            JoinLimits::new(2, usize::MAX),
        ),
        Err(CatalogError::InnerJoin(InnerJoinExecutionError::Join(
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
        right_exceeded.execute_inner_join(
            JOIN_SQL,
            ParseLimits::default(),
            JoinLimits::new(2, usize::MAX),
        ),
        Err(CatalogError::InnerJoin(InnerJoinExecutionError::Join(
            JoinError::RightInputLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        )))
    );
}
