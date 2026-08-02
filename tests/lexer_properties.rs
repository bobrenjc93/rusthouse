use std::panic::{AssertUnwindSafe, catch_unwind};

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::string::string_regex;
use proptest::test_runner::{Config, RngAlgorithm, RngSeed, TestCaseError, TestRunner};
use rusthouse::lexer::{LexerLimits, lex};

const CASES: u32 = 512;
const MAX_INPUT_CHARS: usize = 256;
const MAX_INPUT_BYTES: usize = MAX_INPUT_CHARS * 4;
const MAX_LIMIT_TOKENS: usize = 128;
const MAX_VALID_TOKENS: usize = 64;
const VALID_SQL_LIMITS: LexerLimits = LexerLimits::new(16 * 1024, MAX_VALID_TOKENS, 16 * 1024);

fn runner(seed: u64) -> TestRunner {
    TestRunner::new(Config {
        cases: CASES,
        failure_persistence: None,
        max_shrink_iters: 2_048,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(seed),
        ..Config::default()
    })
}

fn arbitrary_utf8_and_limits() -> impl Strategy<Value = (String, LexerLimits)> {
    (
        vec(any::<char>(), 0..=MAX_INPUT_CHARS),
        0..=MAX_INPUT_BYTES,
        0..=MAX_LIMIT_TOKENS,
        0..=MAX_INPUT_BYTES,
    )
        .prop_map(
            |(characters, max_input_bytes, max_tokens, max_literal_bytes)| {
                (
                    characters.into_iter().collect(),
                    LexerLimits::new(max_input_bytes, max_tokens, max_literal_bytes),
                )
            },
        )
}

fn quoted_token(quote: char) -> impl Strategy<Value = String> {
    vec(any::<char>(), 0..=24).prop_map(move |characters| {
        let mut token = String::new();
        token.push(quote);
        for character in characters {
            token.push(character);
            if character == quote {
                token.push(character);
            }
        }
        token.push(quote);
        token
    })
}

fn valid_token() -> BoxedStrategy<String> {
    prop_oneof![
        3 => string_regex("[A-Za-z_][A-Za-z0-9_$]{0,24}")
            .expect("identifier regex is valid"),
        2 => quoted_token('\''),
        2 => quoted_token('"'),
        2 => any::<u64>().prop_map(|number| number.to_string()),
        1 => prop::sample::select(vec![
            "=", "!=", "<>", "<", "<=", ">", ">=", "+", "-", "*", "/", "%", "||",
            "::", "(", ")", "[", "]", ",", ".", ";",
        ])
        .prop_map(str::to_owned),
    ]
    .boxed()
}

fn valid_sql() -> impl Strategy<Value = String> {
    vec(valid_token(), 1..=MAX_VALID_TOKENS).prop_map(|tokens| tokens.join(" "))
}

#[test]
fn arbitrary_utf8_never_panics() {
    runner(0x5EED_5A11)
        .run(&arbitrary_utf8_and_limits(), |(sql, limits)| {
            let _ = catch_unwind(AssertUnwindSafe(|| lex(&sql, limits)))
                .map_err(|_| TestCaseError::fail("lexer panicked"))?;

            Ok(())
        })
        .expect("bounded deterministic lexer panic-safety property");
}

#[test]
fn valid_sql_spans_are_nonempty_ordered_non_overlapping_and_in_bounds() {
    runner(0x5EED_5A12)
        .run(&valid_sql(), |sql| {
            let tokens = lex(&sql, VALID_SQL_LIMITS).map_err(|error| {
                TestCaseError::fail(format!("valid SQL failed to lex: {error}"))
            })?;

            prop_assert!(!tokens.is_empty());

            let mut previous_end = 0;
            for token in tokens {
                prop_assert!(token.span.start < token.span.end);
                prop_assert!(token.span.start >= previous_end);
                prop_assert!(token.span.end <= sql.len());
                prop_assert!(sql.is_char_boundary(token.span.start));
                prop_assert!(sql.is_char_boundary(token.span.end));
                previous_end = token.span.end;
            }

            Ok(())
        })
        .expect("bounded deterministic lexer span-invariant property");
}

#[test]
fn repeated_lexing_returns_the_same_tokens_or_typed_error() {
    runner(0x5EED_DE7E)
        .run(&arbitrary_utf8_and_limits(), |(sql, limits)| {
            let first = catch_unwind(AssertUnwindSafe(|| lex(&sql, limits)))
                .map_err(|_| TestCaseError::fail("first lexer call panicked"))?;
            let second = catch_unwind(AssertUnwindSafe(|| lex(&sql, limits)))
                .map_err(|_| TestCaseError::fail("second lexer call panicked"))?;

            prop_assert_eq!(first, second);
            Ok(())
        })
        .expect("bounded deterministic lexer repeatability property");
}
