use rusthouse::{
    Catalog, CatalogError, CatalogLimits, Int64Table, ParseError, ParseLimits, Schema,
    SelectExecutionError, execute_select, parse_select,
};

fn table(values: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("value", true), values.len());
    table.append_batch(values).unwrap();
    table
}

#[test]
fn adds_in_source_order_and_propagates_nulls_at_integer_extremes() {
    let table = table(&[Some(i64::MIN), None, Some(-1), Some(i64::MAX)]);
    let statement = parse_select(
        "SELECT value + 0 FROM readings LIMIT 4;",
        ParseLimits::default(),
    )
    .unwrap();

    assert_eq!(
        execute_select("readings", &table, &statement)
            .unwrap()
            .as_ref(),
        &[Some(i64::MIN), None, Some(-1), Some(i64::MAX)]
    );
}

#[test]
fn reports_typed_positive_and_negative_overflow() {
    for (value, addend) in [(i64::MAX, 1), (i64::MIN, -1)] {
        let table = table(&[Some(value)]);
        let statement = parse_select(
            &format!("SELECT value + {addend} FROM readings"),
            ParseLimits::default(),
        )
        .unwrap();

        assert_eq!(
            execute_select("readings", &table, &statement),
            Err(SelectExecutionError::Int64AdditionOverflow { value, addend })
        );
    }
}

#[test]
fn evaluates_only_rows_selected_by_zero_and_exact_limits() {
    let table = table(&[Some(4), None, Some(i64::MAX)]);

    for (limit, expected) in [(0, &[][..]), (2, &[Some(5), None][..])] {
        let statement = parse_select(
            &format!("SELECT value + 1 FROM readings LIMIT {limit}"),
            ParseLimits::default(),
        )
        .unwrap();
        assert_eq!(
            execute_select("readings", &table, &statement)
                .unwrap()
                .as_ref(),
            expected,
            "LIMIT {limit}"
        );
    }

    let statement = parse_select(
        "SELECT value + 1 FROM readings LIMIT 3",
        ParseLimits::default(),
    )
    .unwrap();
    assert_eq!(
        execute_select("readings", &table, &statement),
        Err(SelectExecutionError::Int64AdditionOverflow {
            value: i64::MAX,
            addend: 1,
        })
    );
}

#[test]
fn validates_table_and_projection_identifiers_before_empty_evaluation() {
    let table = table(&[Some(1)]);
    let cases = [
        (
            "SELECT value + 1 FROM other LIMIT 0",
            SelectExecutionError::UnknownTable {
                name: "other".to_owned(),
            },
        ),
        (
            "SELECT other + 1 FROM readings LIMIT 0",
            SelectExecutionError::UnknownColumn {
                name: "other".to_owned(),
            },
        ),
    ];

    for (input, expected) in cases {
        let statement = parse_select(input, ParseLimits::default()).unwrap();
        assert_eq!(
            execute_select("readings", &table, &statement),
            Err(expected)
        );
    }
}

#[test]
fn executes_addition_through_the_catalog_sql_boundary() {
    let limits = ParseLimits::default();
    let mut catalog = Catalog::new(CatalogLimits::new(1, 3));
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", limits)
        .unwrap();
    for input in [
        "INSERT INTO readings VALUES (2)",
        "INSERT INTO readings VALUES (NULL)",
        "INSERT INTO readings VALUES (4)",
    ] {
        catalog.execute_insert(input, limits).unwrap();
    }

    assert_eq!(
        catalog
            .execute_select("SELECT value + 3 FROM readings LIMIT 3;", limits)
            .unwrap()
            .as_ref(),
        &[Some(5), None, Some(7)]
    );
    assert!(matches!(
        catalog.execute_select("SELECT value + nope FROM readings", limits),
        Err(CatalogError::Parse(ParseError::InvalidInt64 { .. }))
    ));
}
