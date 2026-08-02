//! Streaming result formats.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};

/// An error produced while writing [`CsvWithNamesWriter`] output.
#[derive(Debug)]
pub enum CsvWithNamesError {
    /// The destination writer failed.
    Io(io::Error),
    /// A result row did not contain one value for every column.
    RowWidth {
        /// The number of columns in the header.
        expected: usize,
        /// The number of values in the rejected row.
        actual: usize,
    },
}

impl fmt::Display for CsvWithNamesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "failed to write CSVWithNames output: {error}"),
            Self::RowWidth { expected, actual } => write!(
                formatter,
                "CSVWithNames row has {actual} values, but the header has {expected} columns"
            ),
        }
    }
}

impl Error for CsvWithNamesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::RowWidth { .. } => None,
        }
    }
}

impl From<io::Error> for CsvWithNamesError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Writes ClickHouse-style `CSVWithNames` output without collecting the result.
///
/// Construction writes the column-name record. Each call to [`Self::write_row`]
/// then validates and writes one result row. Every string field is quoted so
/// leading whitespace, trailing whitespace, and empty strings round-trip.
/// Embedded quotes are doubled.
///
/// # Examples
///
/// ```
/// use rusthouse::CsvWithNamesWriter;
///
/// let mut output = Vec::new();
/// let mut csv = CsvWithNamesWriter::new(&mut output, ["city", "description"])?;
/// csv.write_row(["Seattle", "rain, then sun"])?;
/// csv.write_row(["New York", "the \"Big Apple\""])?;
/// csv.flush()?;
///
/// assert_eq!(
///     String::from_utf8(output)?,
///     "\"city\",\"description\"\n\"Seattle\",\"rain, then sun\"\n\"New York\",\"the \"\"Big Apple\"\"\"\n"
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct CsvWithNamesWriter<W> {
    writer: W,
    column_count: usize,
}

impl<W: Write> CsvWithNamesWriter<W> {
    /// Creates a writer and immediately emits its column-name record.
    pub fn new<N, S>(writer: W, column_names: N) -> Result<Self, CsvWithNamesError>
    where
        N: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut csv = Self {
            writer,
            column_count: 0,
        };

        for column_name in column_names {
            if csv.column_count != 0 {
                csv.writer.write_all(b",")?;
            }
            write_field(&mut csv.writer, column_name.as_ref())?;
            csv.column_count += 1;
        }
        csv.writer.write_all(b"\n")?;

        Ok(csv)
    }

    /// Validates and emits one result row.
    ///
    /// The exact-size requirement lets this method reject a malformed row
    /// before any part of that row reaches the destination.
    pub fn write_row<R, S>(&mut self, row: R) -> Result<(), CsvWithNamesError>
    where
        R: IntoIterator<Item = S>,
        R::IntoIter: ExactSizeIterator,
        S: AsRef<str>,
    {
        let row = row.into_iter();
        let actual = row.len();
        if actual != self.column_count {
            return Err(CsvWithNamesError::RowWidth {
                expected: self.column_count,
                actual,
            });
        }

        for (index, value) in row.enumerate() {
            if index != 0 {
                self.writer.write_all(b",")?;
            }
            write_field(&mut self.writer, value.as_ref())?;
        }
        self.writer.write_all(b"\n")?;
        Ok(())
    }

    /// Flushes the underlying writer.
    pub fn flush(&mut self) -> Result<(), CsvWithNamesError> {
        self.writer.flush()?;
        Ok(())
    }

    /// Returns the underlying writer without flushing it.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

fn write_field(writer: &mut impl Write, field: &str) -> io::Result<()> {
    writer.write_all(b"\"")?;
    let mut remainder = field;
    while let Some(quote_index) = remainder.find('"') {
        let (before_quote, after_quote) = remainder.split_at(quote_index);
        writer.write_all(before_quote.as_bytes())?;
        writer.write_all(b"\"\"")?;
        remainder = &after_quote[1..];
    }
    writer.write_all(remainder.as_bytes())?;
    writer.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_header_before_rows_are_supplied() {
        let mut output = Vec::new();
        {
            let _csv = CsvWithNamesWriter::new(&mut output, ["first", "second"]).unwrap();
        }

        assert_eq!(output, b"\"first\",\"second\"\n");
    }

    #[test]
    fn quotes_every_csv_boundary_character() {
        let mut csv = CsvWithNamesWriter::new(
            Vec::new(),
            [
                "plain",
                "with,comma",
                "with\"quote",
                "with\rreturn",
                "with\nline",
            ],
        )
        .unwrap();

        csv.write_row(["", ",", "\"", "\r", "\r\n"]).unwrap();

        assert_eq!(
            String::from_utf8(csv.into_inner()).unwrap(),
            concat!(
                "\"plain\",\"with,comma\",\"with\"\"quote\",\"with\rreturn\",\"with\nline\"\n",
                "\"\",\",\",\"\"\"\",\"\r\",\"\r\n\"\n"
            )
        );
    }

    #[test]
    fn round_trips_boundary_whitespace_and_a_single_column_empty_value() {
        let fields = ["value", " padded ", "\ttabbed\t", ""];
        let mut csv = CsvWithNamesWriter::new(Vec::new(), [fields[0]]).unwrap();
        for field in &fields[1..] {
            csv.write_row([field]).unwrap();
        }

        let output = String::from_utf8(csv.into_inner()).unwrap();
        let decoded: Vec<_> = output
            .lines()
            .map(|record| {
                record
                    .strip_prefix('"')
                    .and_then(|record| record.strip_suffix('"'))
                    .expect("single string field should be quoted")
                    .replace("\"\"", "\"")
            })
            .collect();

        assert_eq!(decoded, fields);
        assert!(output.ends_with("\"\"\n"));
    }

    #[test]
    fn rejects_rows_on_both_sides_of_the_expected_width_without_writing_them() {
        let mut csv = CsvWithNamesWriter::new(Vec::new(), ["a", "b"]).unwrap();

        assert!(matches!(
            csv.write_row(["one"]),
            Err(CsvWithNamesError::RowWidth {
                expected: 2,
                actual: 1
            })
        ));
        assert!(matches!(
            csv.write_row(["one", "two", "three"]),
            Err(CsvWithNamesError::RowWidth {
                expected: 2,
                actual: 3
            })
        ));
        csv.write_row(["one", "two"]).unwrap();

        assert_eq!(csv.into_inner(), b"\"a\",\"b\"\n\"one\",\"two\"\n");
    }

    #[test]
    fn handles_the_zero_column_boundary() {
        let mut csv = CsvWithNamesWriter::new(Vec::new(), std::iter::empty::<&str>()).unwrap();

        csv.write_row(std::iter::empty::<&str>()).unwrap();
        assert!(matches!(
            csv.write_row(["unexpected"]),
            Err(CsvWithNamesError::RowWidth {
                expected: 0,
                actual: 1
            })
        ));

        assert_eq!(csv.into_inner(), b"\n\n");
    }

    #[test]
    fn preserves_writer_errors_as_a_typed_source() {
        #[derive(Debug)]
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("destination full"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let error = CsvWithNamesWriter::new(FailingWriter, ["column"]).unwrap_err();
        assert!(matches!(error, CsvWithNamesError::Io(_)));
        assert_eq!(error.source().unwrap().to_string(), "destination full");
    }
}
