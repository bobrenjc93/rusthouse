use std::cmp::Ordering;

use super::error::{Error, Result};

pub(super) fn lower_cmp(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}

pub(super) fn upper_cmp(left: &str, right: &str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_uppercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_uppercase()))
}

pub(super) fn byte_len_to_i64(length: usize) -> Result<i64> {
    i64::try_from(length).map_err(|_| Error::NumericOverflow("LENGTH(String)".to_owned()))
}

pub(super) fn scalar_len_to_i64(value: &str) -> Result<i64> {
    i64::try_from(value.chars().count())
        .map_err(|_| Error::NumericOverflow("lengthUTF8(String)".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: [&str; 12] = [
        "",
        "A",
        "a",
        "Alpha-09_Z",
        "alpha-09_z",
        "Zebra",
        "zebra",
        "é東京",
        "éA",
        "éa",
        "e\u{301}",
        "👨‍👩‍👧‍👦",
    ];

    #[test]
    fn case_comparisons_match_allocating_ascii_normalization() {
        for left in CASES {
            for right in CASES {
                assert_eq!(
                    lower_cmp(left, right),
                    left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase()),
                    "lowercase comparison of {left:?} and {right:?}"
                );
                assert_eq!(
                    upper_cmp(left, right),
                    left.to_ascii_uppercase().cmp(&right.to_ascii_uppercase()),
                    "uppercase comparison of {left:?} and {right:?}"
                );
            }
        }

        assert_eq!(lower_cmp("a", "A"), Ordering::Equal);
        assert_eq!(upper_cmp("z", "Z"), Ordering::Equal);
    }

    #[test]
    fn length_conversions_match_string_byte_and_scalar_counts() {
        for value in CASES {
            assert_eq!(
                byte_len_to_i64(value.len()),
                Ok(i64::try_from(value.len()).unwrap())
            );
            assert_eq!(
                scalar_len_to_i64(value),
                Ok(i64::try_from(value.chars().count()).unwrap()),
                "scalar length of {value:?}"
            );
        }
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn byte_length_overflow_retains_the_sql_context() {
        let overflow = usize::try_from(i64::MAX).unwrap() + 1;

        assert_eq!(
            byte_len_to_i64(overflow),
            Err(Error::NumericOverflow("LENGTH(String)".to_owned()))
        );
    }
}
