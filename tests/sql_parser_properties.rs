use rusthouse::{ParseError, parse_create_table, parse_insert, parse_select};

const INPUT_ALPHABET: &[char] = &[
    ' ', '\t', '\n', '\r', '(', ')', ',', ';', '*', '\'', '=', '!', '<', '>', '+', '-', '.', '_',
    '0', '1', '9', 'A', 'Z', 'a', 'z', 'S', 'E', 'L', 'C', 'T', 'λ', 'é', '\0',
];

#[test]
fn seeded_arbitrary_utf8_inputs_never_panic_and_report_valid_offsets() {
    let mut state = 0x243f_6a88_85a3_08d3_u64;

    for case in 0..10_000 {
        state = next_state(state);
        let length = (state as usize) % 129;
        let mut input = String::new();
        for _ in 0..length {
            state = next_state(state);
            input.push(INPUT_ALPHABET[(state as usize) % INPUT_ALPHABET.len()]);
        }

        assert_error_offset(&input, parse_create_table(&input), case, "CREATE");
        assert_error_offset(&input, parse_insert(&input), case, "INSERT");
        assert_error_offset(&input, parse_select(&input), case, "SELECT");
    }
}

fn assert_error_offset<T>(input: &str, result: Result<T, ParseError>, case: usize, parser: &str) {
    if let Err(error) = result {
        assert!(
            error.position <= input.len(),
            "{parser} case {case} returned byte {} for {} input bytes",
            error.position,
            input.len(),
        );
        assert!(
            input.is_char_boundary(error.position),
            "{parser} case {case} returned a non-UTF-8 boundary at byte {} for {input:?}",
            error.position,
        );
    }
}

const fn next_state(state: u64) -> u64 {
    state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}
