use rusthouse::{DataType, ParseError, ParseLimits, parse_create_table};

#[test]
fn parses_whitespace_casing_and_nullability_table() {
    let cases = [
        ("CREATE TABLE events (value Int64)", "events", "value", true),
        (
            "  create\ttable\nMetrics\r(Reading\tint64 null)  ",
            "Metrics",
            "Reading",
            true,
        ),
        (
            "CrEaTe TABLE _hourly (event_2 INT64 NoT NuLl)",
            "_hourly",
            "event_2",
            false,
        ),
        (
            "cReAtE TABLE NullableReadings (Measurement nUlLaBlE(iNt64))",
            "NullableReadings",
            "Measurement",
            true,
        ),
    ];

    for (input, table_name, column_name, nullable) in cases {
        let statement = parse_create_table(input, ParseLimits::default()).unwrap();

        assert_eq!(statement.table_name().as_str(), table_name, "{input:?}");
        assert!(!statement.if_not_exists(), "{input:?}");
        assert_eq!(statement.column().name().as_str(), column_name, "{input:?}");
        assert_eq!(statement.column().data_type(), DataType::Int64, "{input:?}");
        assert_eq!(statement.column().is_nullable(), nullable, "{input:?}");
    }
}

#[test]
fn parses_if_not_exists_with_keyword_casing_and_preserves_if_table_name() {
    let statement = parse_create_table(
        "cReAtE tAbLe iF nOt eXiStS Events (value Int64)",
        ParseLimits::default(),
    )
    .unwrap();
    assert!(statement.if_not_exists());
    assert_eq!(statement.table_name().as_str(), "Events");

    let ordinary =
        parse_create_table("CREATE TABLE IF (value Int64)", ParseLimits::default()).unwrap();
    assert!(!ordinary.if_not_exists());
    assert_eq!(ordinary.table_name().as_str(), "IF");
}

#[test]
fn parses_optional_memory_engine_for_plain_and_conditional_create() {
    for (input, if_not_exists) in [
        ("CREATE TABLE events (value Int64) ENGINE = Memory", false),
        (
            "create table if not exists Events (value int64) engine=memory",
            true,
        ),
        ("CrEaTe TABLE metrics (value INT64) EnGiNe = MeMoRy", false),
    ] {
        let statement = parse_create_table(input, ParseLimits::default()).unwrap();
        assert_eq!(statement.if_not_exists(), if_not_exists, "{input:?}");
        assert_eq!(statement.column().data_type(), DataType::Int64, "{input:?}");
    }
}

#[test]
fn rejects_invalid_or_trailing_create_table_engine_clauses() {
    for input in [
        "CREATE TABLE events (value Int64) ENGINE Memory",
        "CREATE TABLE events (value Int64) ENGINE = MergeTree",
        "CREATE TABLE events (value Int64) ENGINE = Memory ENGINE = Memory",
        "CREATE TABLE events (value Int64) ENGINE = Memory trailing",
    ] {
        assert!(
            parse_create_table(input, ParseLimits::default()).is_err(),
            "invalid engine suffix was accepted: {input:?}"
        );
    }
}

#[test]
fn rejects_malformed_if_not_exists_forms() {
    for input in [
        "CREATE TABLE IF EXISTS events (value Int64)",
        "CREATE TABLE IF NOT events (value Int64)",
        "CREATE TABLE IF NOT EXIST events (value Int64)",
        "CREATE TABLE IF NOT EXISTS (value Int64)",
        "CREATE TABLE IF NOT EXISTS events value Int64)",
    ] {
        assert!(
            parse_create_table(input, ParseLimits::default()).is_err(),
            "malformed conditional create was accepted: {input:?}"
        );
    }
}

#[test]
fn accepts_statement_and_identifiers_exactly_at_their_limits() {
    let input = "CREATE TABLE table123 (column45 Int64 NOT NULL)";
    let statement = parse_create_table(input, ParseLimits::new(input.len(), 8)).unwrap();

    assert_eq!(statement.table_name().as_str(), "table123");
    assert_eq!(statement.column().name().as_str(), "column45");
}

#[test]
fn rejects_inputs_over_resource_limits_with_typed_errors() {
    let cases = [
        (
            "CREATE TABLE t (c Int64)",
            ParseLimits::new(23, 8),
            ParseError::StatementTooLong {
                bytes: 24,
                max_bytes: 23,
            },
        ),
        (
            "CREATE TABLE table1234 (c Int64)",
            ParseLimits::new(64, 8),
            ParseError::IdentifierTooLong {
                offset: 13,
                bytes: 9,
                max_bytes: 8,
            },
        ),
        (
            "CREATE TABLE t (column456 Int64)",
            ParseLimits::new(64, 8),
            ParseError::IdentifierTooLong {
                offset: 16,
                bytes: 9,
                max_bytes: 8,
            },
        ),
    ];

    for (input, limits, expected) in cases {
        assert_eq!(
            parse_create_table(input, limits),
            Err(expected),
            "{input:?}"
        );
    }
}

#[test]
fn rejects_malformed_statements_with_byte_offsets() {
    let cases = [
        (
            "",
            ParseError::UnexpectedInput {
                offset: 0,
                expected: "CREATE",
            },
        ),
        (
            "CREAT TABLE t (c Int64)",
            ParseError::UnexpectedInput {
                offset: 0,
                expected: "CREATE",
            },
        ),
        (
            "CREATE TABLE (c Int64)",
            ParseError::UnexpectedInput {
                offset: 13,
                expected: "identifier",
            },
        ),
        (
            "CREATE TABLE t c Int64)",
            ParseError::UnexpectedInput {
                offset: 15,
                expected: "'('",
            },
        ),
        (
            "CREATE TABLE t (c Float64)",
            ParseError::UnexpectedInput {
                offset: 18,
                expected: "Int64",
            },
        ),
        (
            "CREATE TABLE t (c Int64, d Int64)",
            ParseError::UnexpectedInput {
                offset: 23,
                expected: "NULL, NOT NULL, or ')'",
            },
        ),
        (
            "CREATE TABLE t (c Int64 NOT)",
            ParseError::UnexpectedInput {
                offset: 27,
                expected: "whitespace after NOT",
            },
        ),
        (
            "CREATE TABLE t (c Int64 NOT MAYBE)",
            ParseError::UnexpectedInput {
                offset: 28,
                expected: "NULL",
            },
        ),
        (
            "CREATE TABLE t (c Int64",
            ParseError::UnexpectedInput {
                offset: 23,
                expected: "NULL, NOT NULL, or ')'",
            },
        ),
        (
            "CREATE TABLE t (c Int64 NULL NULL)",
            ParseError::UnexpectedInput {
                offset: 29,
                expected: "')'",
            },
        ),
        (
            "CREATE TABLE café (c Int64)",
            ParseError::UnexpectedInput {
                offset: 16,
                expected: "'('",
            },
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(
            parse_create_table(input, ParseLimits::default()),
            Err(expected),
            "{input:?}"
        );
    }
}

#[test]
fn nullable_int64_wrapper_stays_inside_exact_parser_resource_and_shape_boundaries() {
    for input in [
        "CREATE TABLE t (c Nullable(Float64))",
        "CREATE TABLE t (c Nullable())",
        "CREATE TABLE t (c Nullable(Int64, Int64))",
        "CREATE TABLE t (c Nullable((Int64)))",
        "CREATE TABLE t (c Nullable Int64)",
        "CREATE TABLE t (c Nullable(Int64) NULL)",
        "CREATE TABLE t (c Nullable(Int64), d Int64)",
    ] {
        assert!(
            parse_create_table(input, ParseLimits::default()).is_err(),
            "out-of-shape nullable declaration was accepted: {input:?}"
        );
    }

    let input = "CREATE TABLE table123 (column45 Nullable(Int64))";
    let statement = parse_create_table(input, ParseLimits::new(input.len(), 8)).unwrap();
    assert_eq!(statement.table_name().as_str(), "table123");
    assert_eq!(statement.column().name().as_str(), "column45");
    assert!(statement.column().is_nullable());

    assert_eq!(
        parse_create_table(input, ParseLimits::new(input.len() - 1, 8)),
        Err(ParseError::StatementTooLong {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        })
    );
    assert_eq!(
        parse_create_table(input, ParseLimits::new(input.len(), 7)),
        Err(ParseError::IdentifierTooLong {
            offset: 13,
            bytes: 8,
            max_bytes: 7,
        })
    );
}

#[test]
fn rejects_non_whitespace_trailing_input_at_its_first_byte() {
    let cases = [
        ("CREATE TABLE t (c Int64);", 24),
        ("CREATE TABLE t (c Int64) extra", 25),
        ("\nCREATE TABLE t (c Int64)\tSELECT", 26),
    ];

    for (input, offset) in cases {
        assert_eq!(
            parse_create_table(input, ParseLimits::default()),
            Err(ParseError::TrailingInput { offset }),
            "{input:?}"
        );
    }
}
