//! Streaming result formats.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};

/// A failure while writing CSVWithNames output.
#[derive(Debug)]
pub enum CsvError {
    /// The output target rejected a write.
    Io(io::Error),
    /// A record did not contain the same number of fields as the header.
    RowWidth {
        /// Zero-based record index, excluding the header.
        row: usize,
        /// Number of fields in the header.
        expected: usize,
        /// Number of fields in the record.
        actual: usize,
    },
}

impl fmt::Display for CsvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "failed to write CSVWithNames output: {error}"),
            Self::RowWidth {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "CSV record {row} has {actual} fields, but the header requires {expected}"
            ),
        }
    }
}

impl Error for CsvError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::RowWidth { .. } => None,
        }
    }
}

impl From<io::Error> for CsvError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A streaming writer for ClickHouse-compatible `CSVWithNames` output.
///
/// The header is written by [`CsvWithNamesWriter::new`]. Each subsequent
/// record is validated against the header width and written immediately, so
/// memory use is bounded by one record rather than the complete result set.
/// Fields follow RFC 4180 quoting rules and records end with `\r\n`.
pub struct CsvWithNamesWriter<W> {
    target: W,
    width: usize,
    rows_written: usize,
}

impl<W: Write> CsvWithNamesWriter<W> {
    /// Creates a writer and immediately writes the header record.
    pub fn new<H, F>(target: W, headers: H) -> Result<Self, CsvError>
    where
        H: IntoIterator<Item = F>,
        F: AsRef<str>,
    {
        let mut writer = Self {
            target,
            width: 0,
            rows_written: 0,
        };
        writer.width = write_fields(&mut writer.target, headers)?;
        Ok(writer)
    }

    /// Writes one record after checking that it matches the header width.
    pub fn write_record<R, F>(&mut self, record: R) -> Result<(), CsvError>
    where
        R: IntoIterator<Item = F>,
        F: AsRef<str>,
    {
        let mut fields = Vec::with_capacity(self.width);
        let mut actual = 0_usize;
        for field in record {
            if fields.len() < self.width {
                fields.push(field);
            }
            actual = actual.saturating_add(1);
        }

        if actual != self.width {
            return Err(CsvError::RowWidth {
                row: self.rows_written,
                expected: self.width,
                actual,
            });
        }

        write_fields(&mut self.target, fields)?;
        self.rows_written += 1;
        Ok(())
    }

    /// Consumes and writes records one at a time.
    pub fn write_records<R, I, F>(&mut self, records: R) -> Result<(), CsvError>
    where
        R: IntoIterator<Item = I>,
        I: IntoIterator<Item = F>,
        F: AsRef<str>,
    {
        for record in records {
            self.write_record(record)?;
        }
        Ok(())
    }

    /// Returns the number of data records successfully written.
    pub fn rows_written(&self) -> usize {
        self.rows_written
    }

    /// Returns the wrapped output target without flushing it.
    pub fn into_inner(self) -> W {
        self.target
    }
}

/// Streams a header and records to an [`io::Write`] target as CSVWithNames.
///
/// This function does not flush `target`; callers that buffer output retain
/// control over when it is flushed.
pub fn write_csv_with_names<W, H, HF, R, I, F>(
    target: &mut W,
    headers: H,
    records: R,
) -> Result<(), CsvError>
where
    W: Write,
    H: IntoIterator<Item = HF>,
    HF: AsRef<str>,
    R: IntoIterator<Item = I>,
    I: IntoIterator<Item = F>,
    F: AsRef<str>,
{
    let mut writer = CsvWithNamesWriter::new(target, headers)?;
    writer.write_records(records)
}

fn write_fields<W, I, F>(target: &mut W, fields: I) -> io::Result<usize>
where
    W: Write,
    I: IntoIterator<Item = F>,
    F: AsRef<str>,
{
    let mut count = 0;
    for field in fields {
        if count > 0 {
            target.write_all(b",")?;
        }
        write_field(target, field.as_ref())?;
        count += 1;
    }
    target.write_all(b"\r\n")?;
    Ok(count)
}

fn write_field<W: Write>(target: &mut W, field: &str) -> io::Result<()> {
    let bytes = field.as_bytes();
    if !bytes
        .iter()
        .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
    {
        return target.write_all(bytes);
    }

    target.write_all(b"\"")?;
    let mut remaining = bytes;
    while let Some(quote) = remaining.iter().position(|byte| *byte == b'"') {
        target.write_all(&remaining[..quote])?;
        target.write_all(b"\"\"")?;
        remaining = &remaining[quote + 1..];
    }
    target.write_all(remaining)?;
    target.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn empty_result_writes_names_only() {
        let mut output = Vec::new();

        write_csv_with_names(&mut output, ["id", "name"], std::iter::empty::<[&str; 2]>()).unwrap();

        assert_eq!(output, b"id,name\r\n");
    }

    #[test]
    fn quotes_every_rfc_4180_special_character() {
        let mut output = Vec::new();

        write_csv_with_names(
            &mut output,
            ["plain", "with,comma", "with\"quote", "with\rcr", "with\nlf"],
            [["value", "a,b", "say \"hi\"", "a\rb", "a\nb"]],
        )
        .unwrap();

        assert_eq!(
            output,
            b"plain,\"with,comma\",\"with\"\"quote\",\"with\rcr\",\"with\nlf\"\r\n\
              value,\"a,b\",\"say \"\"hi\"\"\",\"a\rb\",\"a\nb\"\r\n"
        );
    }

    #[test]
    fn rejects_short_and_long_records_before_writing_them() {
        for record in [vec!["one"], vec!["one", "two", "three"]] {
            let mut output = Vec::new();
            let mut writer = CsvWithNamesWriter::new(&mut output, ["left", "right"]).unwrap();

            let error = writer.write_record(record).unwrap_err();

            assert!(matches!(
                error,
                CsvError::RowWidth {
                    row: 0,
                    expected: 2,
                    actual: 1 | 3,
                }
            ));
            assert_eq!(output, b"left,right\r\n");
        }
    }

    #[test]
    fn consumes_records_only_as_their_output_is_written() {
        #[derive(Clone)]
        struct SharedTarget(Rc<RefCell<Vec<u8>>>);

        impl Write for SharedTarget {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let bytes = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&bytes);
        let records = (0..2).map(move |row| {
            if row == 1 {
                assert_eq!(&*observed.borrow(), b"name\r\nfirst\r\n");
            }
            [if row == 0 { "first" } else { "second" }]
        });

        write_csv_with_names(&mut SharedTarget(Rc::clone(&bytes)), ["name"], records).unwrap();

        assert_eq!(&*bytes.borrow(), b"name\r\nfirst\r\nsecond\r\n");
    }

    #[test]
    fn returns_output_failures() {
        struct FailingTarget {
            remaining: usize,
        }

        impl Write for FailingTarget {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "target closed"));
                }
                let written = self.remaining.min(bytes.len());
                self.remaining -= written;
                Ok(written)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let error =
            write_csv_with_names(&mut FailingTarget { remaining: 6 }, ["name"], [["record"]])
                .unwrap_err();

        match error {
            CsvError::Io(error) => assert_eq!(error.kind(), io::ErrorKind::BrokenPipe),
            CsvError::RowWidth { .. } => panic!("expected an I/O failure"),
        }
    }
}
