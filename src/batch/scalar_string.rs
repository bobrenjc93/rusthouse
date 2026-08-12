use std::cmp::Ordering;

use super::error::{Error, Result};

pub(super) fn ascii_lower_cmp(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}

pub(super) fn ascii_upper_cmp(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_uppercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_uppercase()))
}

pub(super) fn string_length_to_i64(length: usize) -> Result<i64> {
    i64::try_from(length).map_err(|_| Error::NumericOverflow("LENGTH(String)".to_owned()))
}

pub(super) fn string_length_utf8_to_i64(value: &str) -> Result<i64> {
    i64::try_from(value.chars().count())
        .map_err(|_| Error::NumericOverflow("lengthUTF8(String)".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_case_comparisons_match_allocating_canonical_strings() {
        let cases = [
            "",
            "A",
            "a",
            "Z",
            "[",
            "`",
            "Alpha9",
            "ALPHA",
            "Straße",
            "ÉZ",
            "éz",
            "東京A",
            "e\u{301}",
            "👨‍👩‍👧‍👦",
        ];

        for left in cases {
            for right in cases {
                assert_eq!(
                    ascii_lower_cmp(left, right),
                    left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase()),
                    "LOWER({left:?}) versus LOWER({right:?})"
                );
                assert_eq!(
                    ascii_upper_cmp(left, right),
                    left.to_ascii_uppercase().cmp(&right.to_ascii_uppercase()),
                    "UPPER({left:?}) versus UPPER({right:?})"
                );
            }
        }
    }

    #[test]
    fn string_lengths_match_standard_byte_and_scalar_counts() {
        for value in ["", "ASCII", "é東京", "e\u{301}", "👨‍👩‍👧‍👦"] {
            assert_eq!(
                string_length_to_i64(value.len()),
                i64::try_from(value.len())
                    .map_err(|_| Error::NumericOverflow("LENGTH(String)".to_owned())),
                "byte length for {value:?}"
            );
            assert_eq!(
                string_length_utf8_to_i64(value),
                i64::try_from(value.chars().count())
                    .map_err(|_| Error::NumericOverflow("lengthUTF8(String)".to_owned())),
                "scalar length for {value:?}"
            );
        }
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn byte_length_overflow_preserves_the_sql_context() {
        let overflow = usize::try_from(i64::MAX).unwrap() + 1;
        assert_eq!(
            string_length_to_i64(overflow),
            Err(Error::NumericOverflow("LENGTH(String)".to_owned()))
        );
    }
}
