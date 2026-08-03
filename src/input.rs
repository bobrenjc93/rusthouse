use std::fmt;
use std::io::{self, Read};

/// Maximum SQL batch size accepted by input readers and execution APIs.
pub const MAX_SQL_INPUT_BYTES: usize = 1024 * 1024;

/// An error encountered while reading a SQL batch.
#[derive(Debug)]
pub enum InputError {
    Io(io::Error),
    TooLarge { maximum: usize },
    InvalidUtf8,
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "failed to read SQL input: {error}"),
            Self::TooLarge { maximum } => {
                write!(f, "SQL input exceeds the maximum size of {maximum} bytes")
            }
            Self::InvalidUtf8 => f.write_str("SQL input is not valid UTF-8"),
        }
    }
}

impl std::error::Error for InputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::TooLarge { .. } | Self::InvalidUtf8 => None,
        }
    }
}

/// Reads a SQL batch through the first byte beyond the configured limit.
pub fn read_sql_input<R: Read>(mut reader: R) -> Result<String, InputError> {
    let mut input = Vec::new();
    reader
        .by_ref()
        .take((MAX_SQL_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .map_err(InputError::Io)?;

    if input.len() > MAX_SQL_INPUT_BYTES {
        return Err(InputError::TooLarge {
            maximum: MAX_SQL_INPUT_BYTES,
        });
    }

    String::from_utf8(input).map_err(|_| InputError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor, Read};

    struct CountingReader {
        inner: Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.bytes_read += read;
            Ok(read)
        }
    }

    #[test]
    fn reads_input_at_the_limit() {
        let input = " ".repeat(MAX_SQL_INPUT_BYTES);
        assert_eq!(read_sql_input(input.as_bytes()).unwrap().len(), input.len());
    }

    #[test]
    fn stops_after_the_first_excess_byte() {
        let bytes = vec![b'x'; MAX_SQL_INPUT_BYTES + 17_000];
        let mut reader = CountingReader {
            inner: Cursor::new(bytes),
            bytes_read: 0,
        };

        let error = read_sql_input(&mut reader).unwrap_err();

        assert!(matches!(error, InputError::TooLarge { .. }));
        assert_eq!(reader.bytes_read, MAX_SQL_INPUT_BYTES + 1);
    }
}
