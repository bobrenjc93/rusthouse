use std::cmp::Ordering;
use std::fmt;

const MAX_INT64_TEXT_BYTES: usize = 20;

// Rust's finite `f64` Display form is longest for the negative smallest
// subnormal: `-0.` followed by its fractional decimal digits.
const MAX_FLOAT64_TEXT_BYTES: usize = 327;

pub(super) fn render_int64(value: i64) -> String {
    int64_text(value).as_str().to_owned()
}

pub(super) fn int64_len(value: i64) -> usize {
    let magnitude = value.unsigned_abs();
    let digits = if magnitude == 0 {
        1
    } else {
        magnitude.ilog10() as usize + 1
    };
    digits + usize::from(value.is_negative())
}

pub(super) fn int64_cmp(left: i64, right: i64) -> Ordering {
    int64_text(left).as_str().cmp(int64_text(right).as_str())
}

pub(super) fn render_float64(value: f64) -> String {
    float64_text(value).as_str().to_owned()
}

pub(super) fn float64_len(value: f64) -> usize {
    float64_text(value).len()
}

pub(super) fn float64_cmp(left: f64, right: f64) -> Ordering {
    float64_text(left)
        .as_str()
        .cmp(float64_text(right).as_str())
}

pub(super) fn render_bool(value: bool) -> String {
    bool_text(value).to_owned()
}

pub(super) fn bool_len(value: bool) -> usize {
    bool_text(value).len()
}

pub(super) fn bool_cmp(left: bool, right: bool) -> Ordering {
    bool_text(left).cmp(bool_text(right))
}

struct Int64Text {
    bytes: [u8; MAX_INT64_TEXT_BYTES],
    start: usize,
}

impl Int64Text {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[self.start..]).expect("Int64 text is ASCII")
    }
}

fn int64_text(value: i64) -> Int64Text {
    let mut rendered = Int64Text {
        bytes: [0; MAX_INT64_TEXT_BYTES],
        start: MAX_INT64_TEXT_BYTES,
    };
    let mut magnitude = value.unsigned_abs();
    loop {
        rendered.start -= 1;
        rendered.bytes[rendered.start] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value.is_negative() {
        rendered.start -= 1;
        rendered.bytes[rendered.start] = b'-';
    }
    rendered
}

struct Float64Text {
    bytes: [u8; MAX_FLOAT64_TEXT_BYTES],
    len: usize,
}

impl Float64Text {
    fn len(&self) -> usize {
        self.len
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).expect("Float64 text is ASCII")
    }
}

impl fmt::Write for Float64Text {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
        let destination = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        destination.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn float64_text(value: f64) -> Float64Text {
    debug_assert!(value.is_finite(), "stored Float64 values are finite");
    let mut rendered = Float64Text {
        bytes: [0; MAX_FLOAT64_TEXT_BYTES],
        len: 0,
    };
    fmt::write(&mut rendered, format_args!("{value}"))
        .expect("a finite Float64 decimal fits the bounded text buffer");
    rendered
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int64_representations_lengths_and_order_match_rendered_text() {
        let cases = [
            (i64::MIN, "-9223372036854775808"),
            (-10, "-10"),
            (-1, "-1"),
            (0, "0"),
            (1, "1"),
            (10, "10"),
            (i64::MAX, "9223372036854775807"),
        ];

        for (value, expected) in cases {
            assert_eq!(render_int64(value), expected);
            assert_eq!(int64_len(value), expected.len());
        }
        for (left, _) in cases {
            for (right, _) in cases {
                assert_eq!(
                    int64_cmp(left, right),
                    render_int64(left).cmp(&render_int64(right)),
                    "{left} and {right}"
                );
            }
        }
    }

    #[test]
    fn float64_representations_lengths_and_order_match_rendered_text() {
        let cases = [
            -f64::MAX,
            -12.5,
            -f64::MIN_POSITIVE,
            -f64::from_bits(1),
            -0.0,
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            12.5,
            f64::MAX,
        ];

        for value in cases {
            let expected = value.to_string();
            assert_eq!(render_float64(value), expected);
            assert_eq!(float64_len(value), expected.len());
        }
        assert_eq!(render_float64(-0.0), "-0");
        assert_eq!(render_float64(0.0), "0");

        for left in cases {
            for right in cases {
                assert_eq!(
                    float64_cmp(left, right),
                    render_float64(left).cmp(&render_float64(right)),
                    "{left} and {right}"
                );
            }
        }
    }

    #[test]
    fn bool_representations_lengths_and_order_match_rendered_text() {
        for (value, expected) in [(false, "false"), (true, "true")] {
            assert_eq!(render_bool(value), expected);
            assert_eq!(bool_len(value), expected.len());
        }
        for left in [false, true] {
            for right in [false, true] {
                assert_eq!(
                    bool_cmp(left, right),
                    render_bool(left).cmp(&render_bool(right))
                );
            }
        }
    }
}
