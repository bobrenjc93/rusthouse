use rusthouse::sql::lexer::{
    InvalidNumberReason, LexErrorKind, LexerLimits, Span, Token, TokenKind, tokenize,
};

fn limits(input_bytes: usize, tokens: usize, statements: usize) -> LexerLimits {
    LexerLimits::new(input_bytes, tokens, statements)
}

fn generous_limits(input: &str) -> LexerLimits {
    limits(input.len(), 100, 10)
}

#[test]
fn tokenizes_literals_and_retains_exact_byte_spans() {
    let input = "table_1, 42 3.5 'it''s' TRUE false;";
    let tokens = tokenize(input, generous_limits(input)).unwrap();

    assert_eq!(
        tokens,
        vec![
            Token {
                kind: TokenKind::Identifier("table_1".into()),
                span: Span::new(0, 7),
            },
            Token {
                kind: TokenKind::Comma,
                span: Span::new(7, 8),
            },
            Token {
                kind: TokenKind::Integer("42".into()),
                span: Span::new(9, 11),
            },
            Token {
                kind: TokenKind::Float("3.5".into()),
                span: Span::new(12, 15),
            },
            Token {
                kind: TokenKind::String("it's".into()),
                span: Span::new(16, 23),
            },
            Token {
                kind: TokenKind::Boolean(true),
                span: Span::new(24, 28),
            },
            Token {
                kind: TokenKind::Boolean(false),
                span: Span::new(29, 34),
            },
            Token {
                kind: TokenKind::StatementTerminator,
                span: Span::new(34, 35),
            },
        ]
    );
}

#[test]
fn tokenizes_punctuation_and_comparison_operators() {
    let input = "(a.b * + - / %) = != <> < <= > >=;";
    let tokens = tokenize(input, generous_limits(input)).unwrap();
    let kinds: Vec<_> = tokens.into_iter().map(|token| token.kind).collect();

    assert_eq!(
        kinds,
        vec![
            TokenKind::LeftParen,
            TokenKind::Identifier("a".into()),
            TokenKind::Dot,
            TokenKind::Identifier("b".into()),
            TokenKind::Star,
            TokenKind::Plus,
            TokenKind::Minus,
            TokenKind::Slash,
            TokenKind::Percent,
            TokenKind::RightParen,
            TokenKind::Equal,
            TokenKind::NotEqual,
            TokenKind::NotEqual,
            TokenKind::LessThan,
            TokenKind::LessThanOrEqual,
            TokenKind::GreaterThan,
            TokenKind::GreaterThanOrEqual,
            TokenKind::StatementTerminator,
        ]
    );
}

#[test]
fn handles_empty_escaped_and_unicode_strings() {
    let input = "'' 'a''b' 'cafe\u{301}'";
    let tokens = tokenize(input, generous_limits(input)).unwrap();

    assert_eq!(tokens[0].kind, TokenKind::String(String::new()));
    assert_eq!(tokens[0].span, Span::new(0, 2));
    assert_eq!(tokens[1].kind, TokenKind::String("a'b".into()));
    assert_eq!(tokens[1].span, Span::new(3, 9));
    assert_eq!(tokens[2].kind, TokenKind::String("cafe\u{301}".into()));
    assert_eq!(tokens[2].span, Span::new(10, input.len()));
}

#[test]
fn classifies_decimal_and_exponent_forms_without_losing_source_text() {
    let input = ".5 1. 6e2 7.5E-3";
    let tokens = tokenize(input, generous_limits(input)).unwrap();
    let kinds: Vec<_> = tokens.into_iter().map(|token| token.kind).collect();

    assert_eq!(
        kinds,
        vec![
            TokenKind::Float(".5".into()),
            TokenKind::Float("1.".into()),
            TokenKind::Float("6e2".into()),
            TokenKind::Float("7.5E-3".into()),
        ]
    );
}

#[test]
fn reports_positioned_unterminated_and_invalid_tokens() {
    let unterminated = tokenize("ok 'broken", limits(10, 10, 2)).unwrap_err();
    assert_eq!(unterminated.kind, LexErrorKind::UnterminatedString);
    assert_eq!(unterminated.span, Span::new(3, 10));

    let operator = tokenize("!", limits(1, 1, 1)).unwrap_err();
    assert_eq!(
        operator.kind,
        LexErrorKind::InvalidOperator { character: '!' }
    );
    assert_eq!(operator.span, Span::new(0, 1));

    let character = tokenize("@", limits(1, 1, 1)).unwrap_err();
    assert_eq!(
        character.kind,
        LexErrorKind::UnexpectedCharacter { character: '@' }
    );
    assert_eq!(character.span, Span::new(0, 1));
}

#[test]
fn reports_positioned_invalid_numbers() {
    let exponent = tokenize("12e+", limits(4, 1, 1)).unwrap_err();
    assert_eq!(
        exponent.kind,
        LexErrorKind::InvalidNumber {
            reason: InvalidNumberReason::MissingExponentDigits,
        }
    );
    assert_eq!(exponent.span, Span::new(0, 4));

    let suffix = tokenize("12items", limits(7, 1, 1)).unwrap_err();
    assert_eq!(
        suffix.kind,
        LexErrorKind::InvalidNumber {
            reason: InvalidNumberReason::IdentifierSuffix,
        }
    );
    assert_eq!(suffix.span, Span::new(0, 7));
}

#[test]
fn input_limit_accepts_the_boundary_and_rejects_the_next_byte() {
    let input = "name";
    assert!(tokenize(input, limits(input.len(), 1, 1)).is_ok());

    let error = tokenize(input, limits(input.len() - 1, 1, 1)).unwrap_err();
    assert_eq!(
        error.kind,
        LexErrorKind::InputLimitExceeded {
            limit: 3,
            actual: 4,
        }
    );
    assert_eq!(error.span, Span::new(3, 4));
}

#[test]
fn input_limit_reports_a_complete_utf8_character_span() {
    let input = "\u{e9}";
    let error = tokenize(input, limits(1, 1, 1)).unwrap_err();

    assert_eq!(
        error.kind,
        LexErrorKind::InputLimitExceeded {
            limit: 1,
            actual: 2,
        }
    );
    assert_eq!(error.span, Span::new(0, 2));
}

#[test]
fn token_limit_accepts_the_boundary_and_positions_the_extra_token() {
    let input = "alpha ;";
    assert!(tokenize(input, limits(input.len(), 2, 1)).is_ok());

    let error = tokenize(input, limits(input.len(), 1, 1)).unwrap_err();
    assert_eq!(error.kind, LexErrorKind::TokenLimitExceeded { limit: 1 });
    assert_eq!(error.span, Span::new(6, 7));

    assert!(tokenize("   ", limits(3, 0, 0)).unwrap().is_empty());
}

#[test]
fn statement_limit_counts_only_non_empty_semicolon_delimited_runs() {
    let input = "a b;c;;d";
    assert!(tokenize(input, limits(input.len(), 8, 3)).is_ok());

    let error = tokenize(input, limits(input.len(), 8, 2)).unwrap_err();
    assert_eq!(
        error.kind,
        LexErrorKind::StatementLimitExceeded { limit: 2 }
    );
    assert_eq!(error.span, Span::new(7, 8));

    assert!(tokenize(";;;", limits(3, 3, 0)).is_ok());

    let string_error = tokenize("a;'not scanned'", limits(15, 3, 1)).unwrap_err();
    assert_eq!(
        string_error.kind,
        LexErrorKind::StatementLimitExceeded { limit: 1 }
    );
    assert_eq!(string_error.span, Span::new(2, 3));
}

#[test]
fn limit_error_priority_is_deterministic() {
    let input = "a;b";

    let input_error = tokenize(input, limits(2, 1, 1)).unwrap_err();
    assert!(matches!(
        input_error.kind,
        LexErrorKind::InputLimitExceeded { .. }
    ));

    let token_error = tokenize(input, limits(3, 1, 1)).unwrap_err();
    assert_eq!(
        token_error.kind,
        LexErrorKind::TokenLimitExceeded { limit: 1 }
    );

    let statement_error = tokenize(input, limits(3, 3, 1)).unwrap_err();
    assert_eq!(
        statement_error.kind,
        LexErrorKind::StatementLimitExceeded { limit: 1 }
    );
}
