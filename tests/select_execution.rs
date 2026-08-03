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
fn exposes_query_selections_to_count_and_sum_reductions() {
    let catalog = readings_catalog();

    let filtered = catalog
        .execute_select("SELECT sequence FROM readings WHERE active = true")
        .unwrap();
    assert_eq!(
        filtered.table().count(filtered.selection()),
        Ok(filtered.len())
    );
    assert_eq!(
        filtered.table().sum("sequence", filtered.selection()),
        Ok(Value::Int64(4))
    );

    let unfiltered = catalog
        .execute_select("SELECT value FROM readings")
        .unwrap();
    assert!(unfiltered.selection().is_none());
    assert_eq!(
        unfiltered.table().sum("value", unfiltered.selection()),
        Ok(Value::Float64(2.0))
    );
}

#[test]
fn executes_an_already_parsed_statement() {
    let catalog = readings_catalog();
    let statement = SelectStatement {
        projections: SelectProjection::Columns(vec!["label".to_owned()]),
        table: "Readings".to_owned(),
        predicate: Some(ComparisonPredicate {
            column: "sequence".to_owned(),
            operator: ComparisonOperator::GreaterThanOrEqual,
            value: Value::Int64(2),
        }),
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
