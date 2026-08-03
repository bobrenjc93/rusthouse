use rusthouse::{
    Catalog, CatalogError, CatalogLimits, DistinctError, DistinctLimits, ParseLimits,
    SelectDistinctExecutionError,
};

#[test]
fn executes_select_distinct_sql_with_caller_supplied_limits() {
    let parse_limits = ParseLimits::default();
    let mut catalog = Catalog::new(CatalogLimits::new(1, 4));
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", parse_limits)
        .unwrap();
    for sql in [
        "INSERT INTO readings VALUES (2)",
        "INSERT INTO readings VALUES (NULL)",
        "INSERT INTO readings VALUES (1)",
        "INSERT INTO readings VALUES (2)",
    ] {
        catalog.execute_insert(sql, parse_limits).unwrap();
    }

    assert_eq!(
        catalog.execute_select_distinct(
            "SELECT DISTINCT value FROM readings;",
            parse_limits,
            DistinctLimits::new(4, 3),
        ),
        Ok(vec![None, Some(1), Some(2)])
    );
    assert_eq!(
        catalog.execute_select_distinct(
            "SELECT DISTINCT value FROM readings",
            parse_limits,
            DistinctLimits::new(4, 2),
        ),
        Err(CatalogError::SelectDistinct(
            SelectDistinctExecutionError::Distinct(DistinctError::DistinctValueLimitExceeded {
                values: 3,
                max_values: 2,
            })
        ))
    );
}

#[test]
fn catalog_reports_unknown_distinct_table_and_column() {
    let parse_limits = ParseLimits::default();
    let mut catalog = Catalog::new(CatalogLimits::new(1, 0));
    catalog
        .execute_create("CREATE TABLE readings (value Int64)", parse_limits)
        .unwrap();

    assert_eq!(
        catalog.execute_select_distinct(
            "SELECT DISTINCT value FROM missing",
            parse_limits,
            DistinctLimits::new(0, 0),
        ),
        Err(CatalogError::SelectDistinct(
            SelectDistinctExecutionError::UnknownTable {
                name: "missing".to_owned(),
            }
        ))
    );
    assert_eq!(
        catalog.execute_select_distinct(
            "SELECT DISTINCT other FROM readings",
            parse_limits,
            DistinctLimits::new(0, 0),
        ),
        Err(CatalogError::SelectDistinct(
            SelectDistinctExecutionError::UnknownColumn {
                name: "other".to_owned(),
            }
        ))
    );
}
