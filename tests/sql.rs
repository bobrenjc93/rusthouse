use rusthouse::{MAX_SQL_INPUT_BYTES, SqlError, SqlErrorKind, execute_batch};

fn input_limit_error() -> SqlError {
    SqlError {
        byte_offset: MAX_SQL_INPUT_BYTES + 1,
        kind: SqlErrorKind::LimitExceeded {
            resource: "input byte",
            maximum: MAX_SQL_INPUT_BYTES,
        },
    }
}

#[test]
fn execute_batch_accepts_input_at_the_byte_limit() {
    let mut input = "SELECT 1;".to_owned();
    input.push_str(&" ".repeat(MAX_SQL_INPUT_BYTES - input.len()));

    assert_eq!(input.len(), MAX_SQL_INPUT_BYTES);
    assert_eq!(execute_batch(&input).unwrap().len(), 1);
}

#[test]
fn execute_batch_rejects_oversized_input_before_lexing() {
    let input = "\0".repeat(MAX_SQL_INPUT_BYTES + 1);

    assert_eq!(execute_batch(&input), Err(input_limit_error()));
}

#[test]
fn execute_batch_measures_multibyte_input_in_bytes() {
    let mut input = "SELECT 1;-- ".to_owned();
    input.push_str(&"\u{e9}".repeat((MAX_SQL_INPUT_BYTES - input.len()) / 2));

    assert_eq!(input.len(), MAX_SQL_INPUT_BYTES);
    assert_eq!(execute_batch(&input).unwrap().len(), 1);

    input.push('\u{e9}');
    assert_eq!(input.len(), MAX_SQL_INPUT_BYTES + 2);
    assert_eq!(execute_batch(&input), Err(input_limit_error()));
}
