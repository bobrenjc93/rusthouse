use std::cell::Cell;
use std::error::Error as _;
use std::io::{self, Write};
use std::rc::Rc;

use rusthouse::{CsvError, CsvFormatter, CsvLimits, CsvRecord};

#[test]
fn writes_exact_csv_escaping_and_crlf_record_endings() {
    let formatter = CsvFormatter::new(CsvLimits::new(5, 64));
    let header = ["plain", "comma,name", "quote\"name", "line\r\nname", ""];
    let rows = [[
        "cafe\u{301}",
        "with,comma",
        "say \"hello\"",
        "first\rsecond\nthird",
        "",
    ]];
    let mut output = Vec::new();

    formatter.write(&mut output, &header, rows).unwrap();

    assert_eq!(
        output,
        b"plain,\"comma,name\",\"quote\"\"name\",\"line\r\nname\",\"\"\r\n\
          cafe\xCC\x81,\"with,comma\",\"say \"\"hello\"\"\",\"first\rsecond\nthird\",\"\"\r\n"
    );
}

#[test]
fn accepts_column_and_utf8_cell_limits_at_the_exact_boundary() {
    let formatter = CsvFormatter::new(CsvLimits::new(2, 4));
    let header = ["name", "\u{e9}\u{e9}"];
    let rows = [["1234", "\u{e9}\u{e9}"]];
    let mut output = Vec::new();

    formatter.write(&mut output, &header, rows).unwrap();

    assert_eq!(
        output,
        "name,\u{e9}\u{e9}\r\n1234,\u{e9}\u{e9}\r\n".as_bytes()
    );
}

#[test]
fn rejects_header_and_row_column_counts_above_the_limit() {
    let formatter = CsvFormatter::new(CsvLimits::new(2, 16));
    let mut output = Vec::new();

    let header_error = formatter
        .write(
            &mut output,
            &["a", "b", "c"],
            std::iter::empty::<[&str; 3]>(),
        )
        .unwrap_err();
    assert!(matches!(
        header_error,
        CsvError::ColumnLimitExceeded {
            record: CsvRecord::Header,
            limit: 2,
            actual: 3,
        }
    ));
    assert!(output.is_empty());

    let row_error = formatter
        .write(&mut output, &["a", "b"], [["1", "2", "3"]])
        .unwrap_err();
    assert!(matches!(
        row_error,
        CsvError::ColumnLimitExceeded {
            record: CsvRecord::Row(0),
            limit: 2,
            actual: 3,
        }
    ));
    assert_eq!(output, b"a,b\r\n");
}

#[test]
fn rejects_short_and_long_rows_without_writing_them() {
    let formatter = CsvFormatter::new(CsvLimits::new(3, 16));
    let header = ["a", "b"];

    let mut short_output = Vec::new();
    let short = formatter
        .write(&mut short_output, &header, [["only one"]])
        .unwrap_err();
    assert!(matches!(
        short,
        CsvError::RowWidthMismatch {
            row: 0,
            expected: 2,
            actual: 1,
        }
    ));
    assert_eq!(short_output, b"a,b\r\n");

    let mut long_output = Vec::new();
    let long = formatter
        .write(&mut long_output, &header, [["1", "2", "3"]])
        .unwrap_err();
    assert!(matches!(
        long,
        CsvError::RowWidthMismatch {
            row: 0,
            expected: 2,
            actual: 3,
        }
    ));
    assert_eq!(long_output, b"a,b\r\n");
}

#[test]
fn rejects_oversized_header_and_data_cells_by_unescaped_bytes() {
    let formatter = CsvFormatter::new(CsvLimits::new(2, 4));
    let mut output = Vec::new();

    let header_error = formatter
        .write(
            &mut output,
            &["small", "ok"],
            std::iter::empty::<[&str; 2]>(),
        )
        .unwrap_err();
    assert!(matches!(
        header_error,
        CsvError::CellSizeLimitExceeded {
            record: CsvRecord::Header,
            column: 0,
            limit: 4,
            actual: 5,
        }
    ));
    assert!(output.is_empty());

    let data_error = formatter
        .write(&mut output, &["a", "b"], [["ok", "\u{e9}\u{e9}x"]])
        .unwrap_err();
    assert!(matches!(
        data_error,
        CsvError::CellSizeLimitExceeded {
            record: CsvRecord::Row(0),
            column: 1,
            limit: 4,
            actual: 5,
        }
    ));
    assert_eq!(output, b"a,b\r\n");
}

#[test]
fn propagates_writer_failures_and_stops_before_consuming_rows() {
    let writes = Rc::new(Cell::new(0));
    let mut writer = FailingWriter {
        writes: Rc::clone(&writes),
    };
    let row_pulls = Cell::new(0);
    let rows = std::iter::from_fn(|| {
        row_pulls.set(row_pulls.get() + 1);
        Some(["not reached"])
    });

    let error = CsvFormatter::default()
        .write(&mut writer, &["name"], rows)
        .unwrap_err();

    match &error {
        CsvError::Io(source) => {
            assert_eq!(source.kind(), io::ErrorKind::BrokenPipe);
            assert_eq!(source.to_string(), "destination closed");
        }
        other => panic!("expected writer failure, found {other:?}"),
    }
    assert_eq!(
        error
            .source()
            .and_then(|source| source.downcast_ref::<io::Error>())
            .map(io::Error::kind),
        Some(io::ErrorKind::BrokenPipe)
    );
    assert_eq!(writes.get(), 1);
    assert_eq!(row_pulls.get(), 0);
}

#[test]
fn streams_a_large_default_valid_record_without_an_aggregate_buffer() {
    const COLUMN_COUNT: usize = 128;
    const CELL_BYTES: usize = 8 * 1024;

    let header = vec!["column"; COLUMN_COUNT];
    let quote_heavy_cell = "\"".repeat(CELL_BYTES);
    let row = vec![quote_heavy_cell.as_str(); COLUMN_COUNT];
    let mut writer = RejectAfterHeader::default();

    let error = CsvFormatter::default()
        .write(&mut writer, &header, [row])
        .unwrap_err();

    assert!(matches!(error, CsvError::Io(_)));
    assert!(writer.header_complete);
    assert_eq!(writer.first_rejected_write, Some(1));
}

struct FailingWriter {
    writes: Rc<Cell<usize>>,
}

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        self.writes.set(self.writes.get() + 1);
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "destination closed",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct RejectAfterHeader {
    header_complete: bool,
    first_rejected_write: Option<usize>,
}

impl Write for RejectAfterHeader {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.header_complete {
            self.first_rejected_write.get_or_insert(buffer.len());
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stop after header",
            ));
        }

        if buffer.ends_with(b"\r\n") {
            self.header_complete = true;
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
