use std::panic::{AssertUnwindSafe, catch_unwind};

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, RngSeed, TestCaseError, TestRunner};
use rusthouse::lexer::{LexerLimits, lex};

const CASES: u32 = 512;
const MAX_INPUT_CHARS: usize = 256;
const MAX_INPUT_BYTES: usize = MAX_INPUT_CHARS * 4;
const MAX_TOKENS: usize = 128;

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

fn utf8_sql_and_limits() -> impl Strategy<Value = (String, LexerLimits)> {
    (
        vec(any::<char>(), 0..=MAX_INPUT_CHARS),
        0..=MAX_INPUT_BYTES,
        0..=MAX_TOKENS,
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

#[test]
fn arbitrary_utf8_never_panics_and_successful_spans_are_valid() {
    runner(0x5EED_5A11)
        .run(&utf8_sql_and_limits(), |(sql, limits)| {
            let result = catch_unwind(AssertUnwindSafe(|| lex(&sql, limits)))
                .map_err(|_| TestCaseError::fail("lexer panicked"))?;

            if let Ok(tokens) = result {
                let mut previous_end = 0;
                for token in tokens {
                    prop_assert!(token.span.start <= token.span.end);
                    prop_assert!(token.span.start >= previous_end);
                    prop_assert!(token.span.end <= sql.len());
                    prop_assert!(sql.is_char_boundary(token.span.start));
                    prop_assert!(sql.is_char_boundary(token.span.end));
                    previous_end = token.span.end;
                }
            }

            Ok(())
        })
        .expect("bounded deterministic lexer span property");
}

#[test]
fn repeated_lexing_returns_the_same_tokens_or_typed_error() {
    runner(0x5EED_DE7E)
        .run(&utf8_sql_and_limits(), |(sql, limits)| {
            let first = catch_unwind(AssertUnwindSafe(|| lex(&sql, limits)))
                .map_err(|_| TestCaseError::fail("first lexer call panicked"))?;
            let second = catch_unwind(AssertUnwindSafe(|| lex(&sql, limits)))
                .map_err(|_| TestCaseError::fail("second lexer call panicked"))?;

            prop_assert_eq!(first, second);
            Ok(())
        })
        .expect("bounded deterministic lexer repeatability property");
}
