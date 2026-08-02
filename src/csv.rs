//! CSV result serialization.

use std::borrow::Cow;
use std::io::{self, Write};

use crate::{QueryResult, ScalarValue};

impl ScalarValue {
    fn validate_for_csv(&self) -> io::Result<()> {
        if matches!(self, Self::Float(value) if !value.is_finite()) {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot serialize a non-finite float as CSV",
            ))
        } else {
            Ok(())
        }
    }

    fn csv_value(&self) -> Cow<'_, str> {
        match self {
            Self::Null => Cow::Borrowed("\\N"),
            Self::Integer(value) => Cow::Owned(value.to_string()),
            Self::Float(value) => {
                let mut rendered = value.to_string();
                if !rendered.contains(['.', 'e', 'E']) {
                    rendered.push_str(".0");
                }
                Cow::Owned(rendered)
            }
            Self::Boolean(true) => Cow::Borrowed("true"),
            Self::Boolean(false) => Cow::Borrowed("false"),
            Self::String(value) => Cow::Borrowed(value),
        }
    }
}

/// Writes each query result as a CSV header followed by its single row.
///
/// All values are validated before the writer is touched, so invalid values do
/// not produce partial output.
pub fn write_csv<W: Write>(results: &[QueryResult], mut writer: W) -> io::Result<()> {
    for result in results {
        result.value.validate_for_csv()?;
    }

    for result in results {
        write_csv_field(&mut writer, &result.header)?;
        writer.write_all(b"\n")?;

        write_csv_field(&mut writer, &result.value.csv_value())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn write_csv_field<W: Write>(writer: &mut W, value: &str) -> io::Result<()> {
    let requires_quotes = value.is_empty()
        || value
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'));

    if !requires_quotes {
        return writer.write_all(value.as_bytes());
    }

    writer.write_all(b"\"")?;
    for section in value.split_inclusive('"') {
        writer.write_all(section.as_bytes())?;
        if section.ends_with('"') {
            writer.write_all(b"\"")?;
        }
    }
    writer.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_csv_fields() {
        let results = vec![
            QueryResult {
                header: "message, \"text\"".to_owned(),
                value: ScalarValue::String("one, \"two\"\nthree".to_owned()),
            },
            QueryResult {
                header: "missing".to_owned(),
                value: ScalarValue::Null,
            },
        ];
        let mut output = Vec::new();

        write_csv(&results, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"message, \"\"text\"\"\"\n\"one, \"\"two\"\"\nthree\"\nmissing\n\\N\n"
        );
    }

    #[test]
    fn rejects_non_finite_float_values_before_writing() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let results = vec![QueryResult {
                header: "value".to_owned(),
                value: ScalarValue::Float(value),
            }];
            let mut output = Vec::new();

            let error = write_csv(&results, &mut output).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("non-finite float"));
            assert!(output.is_empty());
        }
    }
}
