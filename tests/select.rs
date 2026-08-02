use rusthouse::csv::write_csv;
use rusthouse::sql::{
    MAX_SQL_BYTES, ParseError, SelectParseLimits, SelectStatement, parse_create_table,
    parse_insert, parse_select, parse_select_with_limits,
};
use rusthouse::{Catalog, CatalogError};

#[test]
fn selects_mixed_rows_by_case_insensitive_name_and_streams_csv() {
    let mut catalog = Catalog::default();
    catalog
        .create_table(
            parse_create_table(
                "CREATE TABLE Events (id Int64, score Float64, active Bool, label String)",
            )
            .unwrap(),
        )
        .unwrap();
    catalog
        .insert(
            parse_insert(
                "INSERT INTO events VALUES (1, 2.5, true, 'first'), (-2, -0.25, false, 'two,too')",
            )
            .unwrap(),
        )
        .unwrap();

    let statement = parse_select("\nSeLeCt\t* FrOm eVeNtS; \r\n").unwrap();
    assert_eq!(
        statement,
        SelectStatement {
            table_name: "eVeNtS".into(),
        }
    );

    let mut output = Vec::new();
    write_csv(catalog.select(statement).unwrap(), &mut output).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        concat!(
            "id,score,active,label\r\n",
            "1,2.5,true,first\r\n",
            "-2,-0.25,false,\"two,too\"\r\n",
        )
    );
}

#[test]
fn selects_an_empty_table_with_its_full_schema() {
    let mut catalog = Catalog::default();
    catalog
        .create_table(parse_create_table("CREATE TABLE empty (id Int64, label String)").unwrap())
        .unwrap();

    let selected = catalog
        .select(parse_select("SELECT * FROM EMPTY").unwrap())
        .unwrap();
    assert!(selected.is_empty());

    let mut output = Vec::new();
    write_csv(selected, &mut output).unwrap();
    assert_eq!(output, b"id,label\r\n");
}

#[test]
fn reports_missing_tables_with_the_query_spelling() {
    let catalog = Catalog::default();

    assert_eq!(
        catalog.select(parse_select("SELECT * FROM MiSsInG").unwrap()),
        Err(CatalogError::TableNotFound {
            name: "MiSsInG".into(),
        })
    );
}

#[test]
fn rejects_malformed_selects_at_the_relevant_byte() {
    let cases = [
        ("", 0, "SELECT"),
        ("SELEC * FROM t", 0, "SELECT"),
        ("SELECT", 6, "'*'"),
        ("SELECT value FROM t", 7, "'*'"),
        ("SELECT *", 8, "FROM"),
        ("SELECT * FORM t", 9, "FROM"),
        ("SELECT * FROM", 13, "table name"),
        ("SELECT * FROM 2bad", 14, "table name"),
    ];

    for (sql, position, expected) in cases {
        let error = parse_select(sql).unwrap_err();
        assert_eq!(error.position(), position, "SQL: {sql:?}");
        assert!(
            matches!(error, ParseError::Syntax { expected: actual, .. } if actual == expected),
            "SQL: {sql:?}, error: {error:?}"
        );
    }
}

#[test]
fn rejects_input_after_the_statement_or_optional_semicolon() {
    for sql in [
        "SELECT * FROM t trailing",
        "SELECT * FROM t; SELECT * FROM other",
        "SELECT * FROM t;;",
    ] {
        let position = if let Some(position) = sql.find("trailing") {
            position
        } else if let Some(position) = sql.find("SELECT * FROM other") {
            position
        } else {
            sql.len() - 1
        };
        assert_eq!(
            parse_select(sql),
            Err(ParseError::TrailingInput { position }),
            "SQL: {sql:?}"
        );
    }
}

#[test]
fn enforces_select_sql_limits_at_the_exact_boundary() {
    let statement = "SELECT * FROM t";
    let at_default_limit = format!("{statement}{}", " ".repeat(MAX_SQL_BYTES - statement.len()));
    assert_eq!(at_default_limit.len(), MAX_SQL_BYTES);
    assert!(parse_select(&at_default_limit).is_ok());

    let over_default_limit = format!("{at_default_limit} ");
    assert_eq!(
        parse_select(&over_default_limit),
        Err(ParseError::SqlTooLarge {
            position: MAX_SQL_BYTES,
            max_bytes: MAX_SQL_BYTES,
            actual_bytes: MAX_SQL_BYTES + 1,
        })
    );

    let limits = SelectParseLimits {
        max_sql_bytes: statement.len(),
    };
    assert!(parse_select_with_limits(statement, limits).is_ok());
    assert_eq!(
        parse_select_with_limits("SELECT * FROM t ", limits),
        Err(ParseError::SqlTooLarge {
            position: statement.len(),
            max_bytes: statement.len(),
            actual_bytes: statement.len() + 1,
        })
    );
}
