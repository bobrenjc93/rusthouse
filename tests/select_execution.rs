use std::error::Error;

use rusthouse::{
    Catalog, CatalogError, CatalogLimits, ComparisonOperator, ComparisonPredicate, DataType,
    ParseErrorKind, ParseLimits, ScanError, SelectParseLimits, SelectProjection, SelectStatement,
    Value,
};

fn readings_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    catalog
        .execute_create(
            "CREATE TABLE Readings (sequence Int64, value Float64, active Bool, label String)",
        )
        .unwrap();
    catalog
        .execute_insert(
            "INSERT INTO readings VALUES \
             (1, -2.5, true, 'first'), \
             (2, 0.0, false, 'second'), \
             (3, 4.5, true, 'third')",
        )
        .unwrap();
    catalog
}

#[test]
fn executes_reordered_all_type_projections_as_a_borrowed_result() {
    let catalog = readings_catalog();
    let result = catalog
        .execute_select("SELECT label, active, value, sequence FROM READINGS")
        .unwrap();

    assert!(std::ptr::eq(
        result.table(),
        catalog.table("readings").unwrap()
    ));
    assert_eq!(
        result
            .fields()
            .map(|field| (field.name(), field.data_type()))
            .collect::<Vec<_>>(),
        [
            ("label", DataType::String),
            ("active", DataType::Bool),
            ("value", DataType::Float64),
            ("sequence", DataType::Int64),
        ]
    );
    assert_eq!(result.row_indices().collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(result.len(), 3);
    assert!(!result.is_empty());
    assert_eq!(result.table().string_column("label").unwrap()[2], "third");
}

#[test]
fn expands_wildcards_and_preserves_duplicate_named_projections() {
    let catalog = readings_catalog();

    let wildcard = catalog.execute_select("SELECT * FROM readings").unwrap();
    assert_eq!(
        wildcard
            .projected_fields()
            .map(|field| field.name())
            .collect::<Vec<_>>(),
        ["sequence", "value", "active", "label"]
    );

    let duplicates = catalog
        .execute_select("SELECT active, sequence, active FROM readings")
        .unwrap();
    assert_eq!(
        duplicates
            .fields()
            .rev()
            .map(|field| field.name())
            .collect::<Vec<_>>(),
        ["active", "sequence", "active"]
    );
}

#[test]
fn filters_rows_and_reports_empty_results() {
    let catalog = readings_catalog();

    let filtered = catalog
        .execute_select("SELECT sequence, label FROM readings WHERE active = true")
        .unwrap();
    assert_eq!(filtered.selected_rows().collect::<Vec<_>>(), [0, 2]);
    assert_eq!(filtered.len(), 2);
    assert!(!filtered.is_empty());

    let no_matches = catalog
        .execute_select("SELECT sequence FROM readings WHERE value > 100.0")
        .unwrap();
    assert_eq!(no_matches.selected_rows().next(), None);
    assert_eq!(no_matches.len(), 0);
    assert!(no_matches.is_empty());

    let mut empty_catalog = Catalog::new();
    empty_catalog
        .execute_create("CREATE TABLE empty (id Int64)")
        .unwrap();
    let empty = empty_catalog.execute_select("SELECT * FROM empty").unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.row_indices().next(), None);
}

#[test]
fn applies_and_before_or_with_or_without_group_parentheses() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE precedence (id Int64, active Bool)")
        .unwrap();
    catalog
        .execute_insert(
            "INSERT INTO precedence VALUES \
             (1, false), (2, true), (2, false), (3, true)",
        )
        .unwrap();

    for where_clause in [
        "id = 1 OR id = 2 AND active = true",
        "(id = 1) OR (id = 2 AND active = true)",
    ] {
        let result = catalog
            .execute_select(&format!("SELECT id FROM precedence WHERE {where_clause}"))
            .unwrap();
        assert_eq!(
            result.row_indices().collect::<Vec<_>>(),
            [0, 1],
            "clause: {where_clause}"
        );
    }
}

#[test]
fn executes_campaign_shaped_groups_across_all_physical_types() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create(
            "CREATE TABLE campaigns (campaign_id Int64, spend Float64, active Bool, channel String)",
        )
        .unwrap();
    catalog
        .execute_insert(
            "INSERT INTO campaigns VALUES \
             (101, 10.0, true, 'email'), \
             (101, 80.0, true, 'search'), \
             (102, 75.0, false, 'search'), \
             (103, 40.0, true, 'search'), \
             (101, 90.0, false, 'email')",
        )
        .unwrap();

    let predicate = "(campaign_id = 101 AND active = true) OR \
                     (channel = 'search' AND spend >= 50.0)";
    let result = catalog
        .execute_select(&format!(
            "SELECT campaign_id FROM campaigns WHERE {predicate}"
        ))
        .unwrap();
    assert_eq!(result.row_indices().collect::<Vec<_>>(), [0, 1, 2]);

    let count = catalog
        .execute_select(&format!(
            "SELECT COUNT(*) AS matches FROM campaigns WHERE {predicate}"
        ))
        .unwrap();
    assert_eq!(count.scalar_value(), Some(&Value::Int64(3)));
}

#[test]
fn unions_intersected_groups_across_packed_bitmap_boundaries() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE boundaries (id Int64)")
        .unwrap();
    catalog
        .table_mut("boundaries")
        .unwrap()
        .insert_batch((0..18).map(|id| vec![Value::Int64(id)]))
        .unwrap();

    let result = catalog
        .execute_select(
            "SELECT id FROM boundaries \
             WHERE (id >= 7 AND id <= 9) OR (id >= 15 AND id <= 17) \
             ORDER BY id DESC LIMIT 5",
        )
        .unwrap();
    assert_eq!(result.row_indices().collect::<Vec<_>>(), [17, 16, 15, 9, 8]);
}

#[test]
fn counts_all_filtered_and_empty_tables_as_one_int64_row() {
    let catalog = readings_catalog();

    let all = catalog
        .execute_select("SELECT COUNT(*) FROM readings")
        .unwrap();
    assert_eq!(
        all.fields()
            .map(|field| (field.name(), field.data_type()))
            .collect::<Vec<_>>(),
        [("count()", DataType::Int64)]
    );
    assert_eq!(all.scalar_value(), Some(&Value::Int64(3)));
    assert_eq!(all.row_indices().collect::<Vec<_>>(), [0]);
    assert_eq!(all.len(), 1);
    assert!(!all.is_empty());

    let filtered = catalog
        .execute_select("SELECT COUNT(*) AS active_count FROM readings WHERE active = true")
        .unwrap();
    assert_eq!(
        filtered
            .fields()
            .map(|field| (field.name(), field.data_type()))
            .collect::<Vec<_>>(),
        [("active_count", DataType::Int64)]
    );
    assert_eq!(filtered.scalar_value(), Some(&Value::Int64(2)));

    let no_matches = catalog
        .execute_select("SELECT COUNT(*) FROM readings WHERE value > 100.0")
        .unwrap();
    assert_eq!(no_matches.scalar_value(), Some(&Value::Int64(0)));
    assert_eq!(no_matches.len(), 1);

    let mut empty_catalog = Catalog::new();
    empty_catalog
        .execute_create("CREATE TABLE empty (id Int64)")
        .unwrap();
    let empty = empty_catalog
        .execute_select("SELECT COUNT(*) AS rows FROM empty")
        .unwrap();
    assert_eq!(empty.scalar_value(), Some(&Value::Int64(0)));
    assert_eq!(empty.len(), 1);
    assert!(!empty.is_empty());
}

#[test]
fn applies_order_validation_and_limit_to_count_results() {
    let catalog = readings_catalog();

    let count = catalog
        .execute_select("SELECT COUNT(*) FROM readings ORDER BY value DESC LIMIT 1")
        .unwrap();
    assert_eq!(count.scalar_value(), Some(&Value::Int64(3)));
    assert_eq!(count.row_indices().collect::<Vec<_>>(), [0]);

    let suppressed = catalog
        .execute_select("SELECT COUNT(*) AS matches FROM readings ORDER BY value LIMIT 0")
        .unwrap();
    assert_eq!(suppressed.fields().next().unwrap().name(), "matches");
    assert_eq!(suppressed.scalar_value(), None);
    assert!(suppressed.is_empty());
    assert_eq!(suppressed.row_indices().next(), None);

    assert_eq!(
        catalog
            .execute_select("SELECT COUNT(*) FROM readings ORDER BY missing")
            .unwrap_err(),
        CatalogError::OrderFieldNotFound {
            name: "missing".to_owned(),
        }
    );
}

#[test]
fn executes_an_already_parsed_statement() {
    let catalog = readings_catalog();
    let statement = SelectStatement {
        projections: SelectProjection::Columns(vec!["label".to_owned()]),
        table: "Readings".to_owned(),
        predicate_groups: vec![vec![ComparisonPredicate {
            column: "sequence".to_owned(),
            operator: ComparisonOperator::GreaterThanOrEqual,
            value: Value::Int64(2),
        }]],
        order_by: None,
        limit: None,
    };

    let result = catalog.select(statement).unwrap();

    assert_eq!(
        result
            .fields()
            .map(|field| field.name())
            .collect::<Vec<_>>(),
        ["label"]
    );
    assert_eq!(result.selected_rows().rev().collect::<Vec<_>>(), [2, 1]);
}

#[test]
fn orders_all_physical_types_and_preserves_source_order_for_ties() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create(
            "CREATE TABLE sortable (id Int64, integer Int64, float Float64, boolean Bool, text String)",
        )
        .unwrap();
    catalog
        .execute_insert(
            "INSERT INTO sortable VALUES \
             (0, 2, 1.5, true, 'bee'), \
             (1, -1, -2.0, false, 'cat'), \
             (2, 2, 1.5, false, 'ant')",
        )
        .unwrap();

    let cases: [(&str, &[usize], &[usize]); 4] = [
        ("integer", &[1, 0, 2], &[0, 2, 1]),
        ("float", &[1, 0, 2], &[0, 2, 1]),
        ("boolean", &[1, 2, 0], &[0, 1, 2]),
        ("text", &[2, 0, 1], &[1, 0, 2]),
    ];

    for (column, ascending, descending) in cases {
        let asc = catalog
            .execute_select(&format!("SELECT id FROM sortable ORDER BY {column}"))
            .unwrap();
        assert_eq!(
            asc.row_indices().collect::<Vec<_>>(),
            ascending,
            "ascending {column}"
        );

        let desc = catalog
            .execute_select(&format!("SELECT id FROM sortable ORDER BY {column} DESC"))
            .unwrap();
        assert_eq!(
            desc.row_indices().collect::<Vec<_>>(),
            descending,
            "descending {column}"
        );
        assert_eq!(desc.len(), 3);
    }
}

#[test]
fn float_order_is_total_and_deterministic() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE floats (id Int64, value Float64)")
        .unwrap();
    catalog
        .table_mut("floats")
        .unwrap()
        .insert_batch([
            vec![Value::Int64(0), Value::Float64(f64::NAN)],
            vec![Value::Int64(1), Value::Float64(0.0)],
            vec![Value::Int64(2), Value::Float64(-0.0)],
            vec![Value::Int64(3), Value::Float64(f64::INFINITY)],
            vec![Value::Int64(4), Value::Float64(f64::NEG_INFINITY)],
            vec![Value::Int64(5), Value::Float64(f64::NAN)],
        ])
        .unwrap();

    let ascending = catalog
        .execute_select("SELECT id FROM floats ORDER BY value ASC")
        .unwrap();
    assert_eq!(
        ascending.row_indices().collect::<Vec<_>>(),
        [4, 2, 1, 3, 0, 5]
    );

    let descending = catalog
        .execute_select("SELECT id FROM floats ORDER BY value DESC")
        .unwrap();
    assert_eq!(
        descending.row_indices().collect::<Vec<_>>(),
        [0, 5, 3, 1, 2, 4]
    );
}

#[test]
fn filters_before_ordering_and_limits_after_ordering() {
    let catalog = readings_catalog();
    let result = catalog
        .execute_select(
            "SELECT label FROM readings WHERE active = true ORDER BY value DESC LIMIT 1",
        )
        .unwrap();

    assert_eq!(result.row_indices().collect::<Vec<_>>(), [2]);
    assert_eq!(result.len(), 1);
    assert!(!result.is_empty());

    let zero = catalog
        .execute_select("SELECT label FROM readings ORDER BY sequence DESC LIMIT 0")
        .unwrap();
    assert!(zero.is_empty());
    assert_eq!(zero.row_indices().next(), None);

    let oversized = catalog
        .execute_select("SELECT label FROM readings ORDER BY sequence DESC LIMIT 100")
        .unwrap();
    assert_eq!(oversized.row_indices().collect::<Vec<_>>(), [2, 1, 0]);
}

#[test]
fn orders_empty_tables_and_reports_missing_order_fields() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE empty (id Int64)")
        .unwrap();

    let empty = catalog
        .execute_select("SELECT * FROM empty ORDER BY id DESC LIMIT 10")
        .unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.row_indices().next(), None);

    assert_eq!(
        catalog
            .execute_select("SELECT id FROM empty ORDER BY missing")
            .unwrap_err(),
        CatalogError::OrderFieldNotFound {
            name: "missing".to_owned(),
        }
    );
}

#[test]
fn limits_unordered_results_after_filtering() {
    let catalog = readings_catalog();
    let result = catalog
        .execute_select("SELECT label FROM readings WHERE sequence > 1 LIMIT 1")
        .unwrap();

    assert_eq!(result.row_indices().collect::<Vec<_>>(), [1]);
    assert_eq!(result.len(), 1);
}

#[test]
fn execute_select_uses_the_catalogs_bounded_parser() {
    let select = "SELECT sequence, active FROM readings";
    let select_limits = SelectParseLimits::new(select.len(), 1);
    let limits =
        CatalogLimits::new(ParseLimits::default(), 1, 10).with_select_parse_limits(select_limits);
    let mut catalog = Catalog::with_limits(limits);
    catalog
        .execute_create("CREATE TABLE readings (sequence Int64, active Bool)")
        .unwrap();

    let error = catalog.execute_select(select).unwrap_err();

    assert!(matches!(
        error,
        CatalogError::Parse(ref parse_error)
            if parse_error.kind == ParseErrorKind::TooManyProjections { limit: 1 }
    ));
    assert!(error.source().is_some());
    assert_eq!(catalog.limits().select_parse, select_limits);
}

#[test]
fn reports_parse_lookup_projection_and_scan_failures_with_typed_errors() {
    let catalog = readings_catalog();

    assert!(matches!(
        catalog.execute_select("SELECT FROM readings"),
        Err(CatalogError::Parse(_))
    ));
    assert_eq!(
        catalog.execute_select("SELECT * FROM missing").unwrap_err(),
        CatalogError::TableNotFound {
            name: "missing".to_owned(),
        }
    );
    assert_eq!(
        catalog
            .execute_select("SELECT missing FROM readings")
            .unwrap_err(),
        CatalogError::ProjectionFieldNotFound {
            name: "missing".to_owned(),
        }
    );

    let missing_predicate = catalog
        .execute_select("SELECT sequence FROM readings WHERE missing = 1")
        .unwrap_err();
    assert_eq!(
        missing_predicate,
        CatalogError::TableScan {
            name: "readings".to_owned(),
            source: ScanError::FieldNotFound {
                name: "missing".to_owned(),
            },
        }
    );
    assert_eq!(
        missing_predicate.source().unwrap().to_string(),
        "field `missing` does not exist"
    );

    assert_eq!(
        catalog
            .execute_select("SELECT sequence FROM readings WHERE sequence = '1'")
            .unwrap_err(),
        CatalogError::TableScan {
            name: "readings".to_owned(),
            source: ScanError::TypeMismatch {
                field: "sequence".to_owned(),
                column_type: DataType::Int64,
                literal_type: DataType::String,
            },
        }
    );
}

#[test]
fn projection_lookup_uses_the_storage_schemas_case_sensitive_names() {
    let catalog = readings_catalog();

    assert_eq!(
        catalog
            .execute_select("SELECT Sequence FROM readings")
            .unwrap_err(),
        CatalogError::ProjectionFieldNotFound {
            name: "Sequence".to_owned(),
        }
    );
}
