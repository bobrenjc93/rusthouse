use rusthouse::sql::lexer::{LexErrorKind, LexerLimits, Span};
use rusthouse::sql::parser::{ExpectedSyntax, ParseErrorKind, parse_create_table};
use rusthouse::{Column, ColumnDef, DataType, Table};

fn parse(input: &str) -> rusthouse::sql::parser::CreateTableStatement {
    parse_create_table(input, LexerLimits::new(input.len(), 100, 10)).unwrap()
}

#[test]
fn parses_keywords_and_all_types_case_insensitively() {
    let statement =
        parse("cReAtE tAbLe Events (id iNt64, score FLOAT64, enabled bool, label string)");

    assert_eq!(statement.name, "Events");
    assert_eq!(
        statement.columns,
        vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("score", DataType::Float64),
            ColumnDef::new("enabled", DataType::Bool),
            ColumnDef::new("label", DataType::String),
        ]
    );
}

#[test]
fn accepts_one_optional_statement_terminator() {
    let without = parse("CREATE TABLE events (id Int64)");
    let with = parse("CREATE TABLE events (id Int64);");

    assert_eq!(without, with);
}

#[test]
fn parsed_schema_constructs_a_typed_table() {
    let statement = parse("CREATE TABLE metrics (id Int64, value Float64)");
    let table = Table::new(statement.name, statement.columns).unwrap();

    assert_eq!(table.name(), "metrics");
    assert_eq!(
        table.schema(),
        &[
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("value", DataType::Float64),
        ]
    );
    assert_eq!(
        table.columns(),
        &[Column::Int64(vec![]), Column::Float64(vec![])]
    );
}

#[test]
fn reports_malformed_syntax_at_the_offending_token_or_end() {
    let cases = [
        (
            "TABLE events (id Int64)",
            ExpectedSyntax::CreateKeyword,
            Span::new(0, 5),
        ),
        (
            "CREATE events (id Int64)",
            ExpectedSyntax::TableKeyword,
            Span::new(7, 13),
        ),
        (
            "CREATE TABLE (id Int64)",
            ExpectedSyntax::TableName,
            Span::new(13, 14),
        ),
        (
            "CREATE TABLE events id Int64)",
            ExpectedSyntax::LeftParenthesis,
            Span::new(20, 22),
        ),
        (
            "CREATE TABLE events ()",
            ExpectedSyntax::ColumnName,
            Span::new(21, 22),
        ),
        (
            "CREATE TABLE events (id)",
            ExpectedSyntax::DataType,
            Span::new(23, 24),
        ),
        (
            "CREATE TABLE events (id Int64 score Float64)",
            ExpectedSyntax::CommaOrRightParenthesis,
            Span::new(30, 35),
        ),
        (
            "CREATE TABLE events (id Int64,)",
            ExpectedSyntax::ColumnName,
            Span::new(30, 31),
        ),
        (
            "CREATE TABLE events (id Int64",
            ExpectedSyntax::CommaOrRightParenthesis,
            Span::new(29, 29),
        ),
    ];

    for (input, expected, span) in cases {
        let error = parse_create_table(input, LexerLimits::default()).unwrap_err();
        assert_eq!(error.kind, ParseErrorKind::Expected(expected), "{input}");
        assert_eq!(error.span, span, "{input}");
    }
}

#[test]
fn distinguishes_unsupported_types_and_trailing_input() {
    let unsupported = "CREATE TABLE events (id UInt64)";
    let error = parse_create_table(unsupported, LexerLimits::default()).unwrap_err();
    assert_eq!(error.kind, ParseErrorKind::UnsupportedType("UInt64".into()));
    assert_eq!(error.span, Span::new(24, 30));

    let trailing = "CREATE TABLE events (id Int64); SELECT";
    let error = parse_create_table(trailing, LexerLimits::default()).unwrap_err();
    assert_eq!(error.kind, ParseErrorKind::TrailingInput);
    assert_eq!(error.span, Span::new(32, 38));

    let extra_terminator = "CREATE TABLE events (id Int64);;";
    let error = parse_create_table(extra_terminator, LexerLimits::default()).unwrap_err();
    assert_eq!(error.kind, ParseErrorKind::TrailingInput);
    assert_eq!(error.span, Span::new(31, 32));
}

#[test]
fn preserves_bounded_lexer_errors_and_spans() {
    let input = "CREATE TABLE events (id Int64)";
    let error = parse_create_table(input, LexerLimits::new(6, 100, 10)).unwrap_err();

    assert_eq!(
        error.kind,
        ParseErrorKind::Lexical(LexErrorKind::InputLimitExceeded {
            limit: 6,
            actual: input.len(),
        })
    );
    assert_eq!(error.span, Span::new(6, input.len()));
}
