use rusthouse::sql::lexer::{
    LexError, LexerLimits, Operator, Punctuation, Span, Token, TokenKind, tokenize,
    tokenize_with_limits,
};

fn kinds(sql: &str) -> Vec<TokenKind> {
    tokenize(sql)
        .expect("SQL should tokenize")
        .into_iter()
        .map(|token| token.kind)
        .collect()
}

#[test]
fn tokenizes_create_table() {
    assert_eq!(
        kinds("CREATE TABLE events (id Int64, active Bool);"),
        vec![
            TokenKind::Identifier("CREATE".into()),
            TokenKind::Identifier("TABLE".into()),
            TokenKind::Identifier("events".into()),
            TokenKind::Punctuation(Punctuation::LeftParen),
            TokenKind::Identifier("id".into()),
            TokenKind::Identifier("Int64".into()),
            TokenKind::Punctuation(Punctuation::Comma),
            TokenKind::Identifier("active".into()),
            TokenKind::Identifier("Bool".into()),
            TokenKind::Punctuation(Punctuation::RightParen),
            TokenKind::Terminator,
        ]
    );
}

#[test]
fn tokenizes_insert_values_and_decodes_escaped_strings() {
    assert_eq!(
        kinds("INSERT INTO events VALUES (1, -2.5e+3, TRUE, 'O''Reilly');"),
        vec![
            TokenKind::Identifier("INSERT".into()),
            TokenKind::Identifier("INTO".into()),
            TokenKind::Identifier("events".into()),
            TokenKind::Identifier("VALUES".into()),
            TokenKind::Punctuation(Punctuation::LeftParen),
            TokenKind::Number("1".into()),
            TokenKind::Punctuation(Punctuation::Comma),
            TokenKind::Operator(Operator::Minus),
            TokenKind::Number("2.5e+3".into()),
            TokenKind::Punctuation(Punctuation::Comma),
            TokenKind::Boolean(true),
            TokenKind::Punctuation(Punctuation::Comma),
            TokenKind::String("O'Reilly".into()),
            TokenKind::Punctuation(Punctuation::RightParen),
            TokenKind::Terminator,
        ]
    );
}

#[test]
fn tokenizes_select_expressions_and_comparison_operators() {
    assert_eq!(
        kinds("SELECT metrics.score / 2 FROM metrics WHERE active = false AND score >= .5 <> 10;",),
        vec![
            TokenKind::Identifier("SELECT".into()),
            TokenKind::Identifier("metrics".into()),
            TokenKind::Punctuation(Punctuation::Dot),
            TokenKind::Identifier("score".into()),
            TokenKind::Operator(Operator::Divide),
            TokenKind::Number("2".into()),
            TokenKind::Identifier("FROM".into()),
            TokenKind::Identifier("metrics".into()),
            TokenKind::Identifier("WHERE".into()),
            TokenKind::Identifier("active".into()),
            TokenKind::Operator(Operator::Equal),
            TokenKind::Boolean(false),
            TokenKind::Identifier("AND".into()),
            TokenKind::Identifier("score".into()),
            TokenKind::Operator(Operator::GreaterEqual),
            TokenKind::Number(".5".into()),
            TokenKind::Operator(Operator::NotEqual),
            TokenKind::Number("10".into()),
            TokenKind::Terminator,
        ]
    );
}

#[test]
fn tokenizes_every_supported_operator() {
    assert_eq!(
        kinds("= != <> < <= > >= + - * / %"),
        vec![
            TokenKind::Operator(Operator::Equal),
            TokenKind::Operator(Operator::NotEqual),
            TokenKind::Operator(Operator::NotEqual),
            TokenKind::Operator(Operator::Less),
            TokenKind::Operator(Operator::LessEqual),
            TokenKind::Operator(Operator::Greater),
            TokenKind::Operator(Operator::GreaterEqual),
            TokenKind::Operator(Operator::Plus),
            TokenKind::Operator(Operator::Minus),
            TokenKind::Operator(Operator::Multiply),
            TokenKind::Operator(Operator::Divide),
            TokenKind::Operator(Operator::Modulo),
        ]
    );
}

#[test]
fn reports_unterminated_string_at_opening_quote() {
    let sql = "INSERT INTO t VALUES ('caf\u{e9}";
    let quote_offset = sql.find('\'').unwrap();

    assert_eq!(
        tokenize(sql),
        Err(LexError::UnterminatedString {
            offset: quote_offset
        })
    );
}

#[test]
fn enforces_input_limit_in_bytes() {
    let error = tokenize_with_limits(
        "SELECT \u{e9}",
        LexerLimits {
            max_input_bytes: 8,
            max_tokens: 10,
        },
    )
    .unwrap_err();

    assert_eq!(
        error,
        LexError::InputTooLong {
            offset: 8,
            actual_bytes: 9,
            max_bytes: 8,
        }
    );
    assert_eq!(error.offset(), 8);
}

#[test]
fn enforces_token_limit_at_the_first_excess_token() {
    let error = tokenize_with_limits(
        "SELECT id FROM events",
        LexerLimits {
            max_input_bytes: 100,
            max_tokens: 2,
        },
    )
    .unwrap_err();

    assert_eq!(
        error,
        LexError::TooManyTokens {
            offset: 10,
            max_tokens: 2,
        }
    );
}

#[test]
fn accepts_values_exactly_at_both_limits() {
    let sql = "SELECT;";
    let tokens = tokenize_with_limits(
        sql,
        LexerLimits {
            max_input_bytes: sql.len(),
            max_tokens: 2,
        },
    )
    .unwrap();

    assert_eq!(
        tokens,
        vec![
            Token {
                kind: TokenKind::Identifier("SELECT".into()),
                span: Span { start: 0, end: 6 },
            },
            Token {
                kind: TokenKind::Terminator,
                span: Span { start: 6, end: 7 },
            },
        ]
    );
}

#[test]
fn reports_malformed_exponents_and_unrecognized_characters() {
    assert_eq!(
        tokenize("SELECT 1e+;"),
        Err(LexError::InvalidNumber { offset: 8 })
    );
    assert_eq!(
        tokenize("SELECT @name"),
        Err(LexError::UnexpectedCharacter {
            offset: 7,
            character: '@',
        })
    );
}
