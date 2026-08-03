use rusthouse::{CsvIngestError, CsvIngestLimits, DataType, Field, Table, TableError, Value};
use std::io::{self, Read};

fn full_schema() -> Vec<Field> {
    vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
    ]
}

#[test]
fn imports_all_types_and_rfc4180_quoted_fields() {
    let mut table = Table::new(full_schema()).unwrap();
    let csv = concat!(
        "integer,float,boolean,text\r\n",
        "-42,3.5,true,\"hello, \"\"RustHouse\"\"\"\r\n",
        "7,-0.25,false,\"line one\nline two\"\r\n",
    );

    assert_eq!(table.insert_csv(csv.as_bytes()).unwrap(), 2);
    assert_eq!(table.int64_column("integer").unwrap(), [-42, 7]);
    assert_eq!(table.float64_column("float").unwrap(), [3.5, -0.25]);
    assert_eq!(
        table.bool_column("boolean").unwrap().collect::<Vec<_>>(),
        [true, false]
    );
    assert_eq!(
        table.string_column("text").unwrap(),
        ["hello, \"RustHouse\"", "line one\nline two"]
    );
}

#[test]
fn requires_the_exact_schema_header() {
    let mut table = Table::new(full_schema()).unwrap();

    let error = table
        .insert_csv(b"float,integer,boolean,text\n3.5,1,true,event\n".as_slice())
        .unwrap_err();

    assert!(matches!(
        error,
        CsvIngestError::HeaderMismatch {
            expected_fields: 4,
            actual_fields: 4,
            first_mismatch: Some(0),
        }
    ));
    assert!(table.is_empty());
}

#[test]
fn quoted_header_delimiters_are_compared_as_one_schema_field() {
    let mut table = Table::new(vec![Field::new("comma,name", DataType::String)]).unwrap();

    assert_eq!(
        table
            .insert_csv(b"\"comma,name\"\n\"quoted,value\"\n".as_slice())
            .unwrap(),
        1
    );
    assert_eq!(table.string_column("comma,name").unwrap(), ["quoted,value"]);
}

#[test]
fn malformed_row_rolls_back_valid_preceding_rows() {
    let mut table = seeded_table();
    let error = table
        .insert_csv(
            concat!(
                "integer,float,boolean,text\n",
                "1,1.5,true,valid\n",
                "2,2.5,false\n",
            )
            .as_bytes(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        CsvIngestError::RowWidthMismatch {
            row: 1,
            expected: 4,
            actual: 3,
        }
    ));
    assert_seed_unchanged(&table);
}

#[test]
fn invalid_typed_value_rolls_back_the_complete_import() {
    let mut table = seeded_table();
    let error = table
        .insert_csv(
            concat!(
                "integer,float,boolean,text\n",
                "1,1.5,true,valid\n",
                "2,2.5,TRUE,invalid\n",
            )
            .as_bytes(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        CsvIngestError::InvalidValue {
            row: 1,
            column: 2,
            expected: DataType::Bool,
            ref value,
            ..
        } if value == "TRUE"
    ));
    assert_seed_unchanged(&table);
}

#[test]
fn byte_limit_accepts_exact_boundary_and_rejects_next_byte() {
    let csv = b"integer,float,boolean,text\n1,2.5,true,ok\n";
    let exact_limits = CsvIngestLimits::new(csv.len(), 1);
    let mut exact = Table::new(full_schema()).unwrap();
    assert_eq!(
        exact
            .insert_csv_with_limits(csv.as_slice(), exact_limits)
            .unwrap(),
        1
    );

    let mut too_small = Table::new(full_schema()).unwrap();
    let error = too_small
        .insert_csv_with_limits(csv.as_slice(), CsvIngestLimits::new(csv.len() - 1, 1))
        .unwrap_err();
    assert!(matches!(
        error,
        CsvIngestError::ByteLimitExceeded { limit } if limit == csv.len() - 1
    ));
    assert!(too_small.is_empty());
}

#[test]
fn row_limit_accepts_exact_boundary_and_rolls_back_on_excess() {
    let csv = b"id\n1\n2\n";
    let mut exact = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();
    assert_eq!(
        exact
            .insert_csv_with_limits(csv.as_slice(), CsvIngestLimits::new(csv.len(), 2))
            .unwrap(),
        2
    );

    let mut limited = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();
    let error = limited
        .insert_csv_with_limits(csv.as_slice(), CsvIngestLimits::new(csv.len(), 1))
        .unwrap_err();
    assert!(matches!(
        error,
        CsvIngestError::RowLimitExceeded { limit: 1 }
    ));
    assert!(limited.is_empty());
}

#[test]
fn decoded_memory_limit_bounds_wide_empty_string_rows_at_exact_boundary() {
    const FIELD_COUNT: usize = 256;

    let fields = (0..FIELD_COUNT)
        .map(|index| Field::new(format!("field_{index}"), DataType::String))
        .collect::<Vec<_>>();
    let header = fields.iter().map(Field::name).collect::<Vec<_>>().join(",");
    let empty_row = ",".repeat(FIELD_COUNT - 1);
    let csv = format!("{header}\n{empty_row}\n{empty_row}\n");
    let decoded_bytes =
        2 * (std::mem::size_of::<Vec<Value>>() + FIELD_COUNT * std::mem::size_of::<Value>());

    let exact_limits = CsvIngestLimits::new(csv.len(), 2).with_max_decoded_bytes(decoded_bytes);
    let mut exact = Table::new(fields.clone()).unwrap();
    assert_eq!(
        exact
            .insert_csv_with_limits(csv.as_bytes(), exact_limits)
            .unwrap(),
        2
    );

    let mut limited = Table::new(fields).unwrap();
    limited
        .insert_batch(vec![vec![Value::String("seed".to_owned()); FIELD_COUNT]])
        .unwrap();
    let error = limited
        .insert_csv_with_limits(
            csv.as_bytes(),
            CsvIngestLimits::new(csv.len(), 2).with_max_decoded_bytes(decoded_bytes - 1),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        CsvIngestError::DecodedLimitExceeded { limit, required }
            if limit == decoded_bytes - 1 && required == decoded_bytes
    ));
    assert_eq!(limited.len(), 1);
    assert_eq!(limited.string_column("field_0").unwrap(), ["seed"]);
    assert_eq!(limited.string_column("field_255").unwrap(), ["seed"]);
}

#[test]
fn table_capacity_failure_from_insert_batch_is_transactional() {
    let mut table = Table::with_row_limit(full_schema(), 2).unwrap();
    table
        .insert_batch(vec![vec![
            Value::Int64(9),
            Value::Float64(9.0),
            Value::Bool(false),
            Value::String("seed".to_owned()),
        ]])
        .unwrap();

    let error = table
        .insert_csv(
            concat!(
                "integer,float,boolean,text\n",
                "1,1.0,true,first\n",
                "2,2.0,false,second\n",
            )
            .as_bytes(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        CsvIngestError::Table(TableError::RowLimitExceeded {
            limit: 2,
            current: 1,
        })
    ));
    assert_seed_unchanged(&table);
}

#[test]
fn late_reader_failure_does_not_mutate_the_table() {
    let mut table = seeded_table();
    let input = FailingReader {
        bytes: b"integer,float,boolean,text\n1,1.0,true,valid\n",
        position: 0,
        fail_at: 35,
    };

    assert!(matches!(
        table.insert_csv(input),
        Err(CsvIngestError::Read(_))
    ));
    assert_seed_unchanged(&table);
}

fn seeded_table() -> Table {
    let mut table = Table::new(full_schema()).unwrap();
    table
        .insert_batch(vec![vec![
            Value::Int64(9),
            Value::Float64(9.0),
            Value::Bool(false),
            Value::String("seed".to_owned()),
        ]])
        .unwrap();
    table
}

fn assert_seed_unchanged(table: &Table) {
    assert_eq!(table.len(), 1);
    assert_eq!(table.int64_column("integer").unwrap(), [9]);
    assert_eq!(table.float64_column("float").unwrap(), [9.0]);
    assert_eq!(
        table.bool_column("boolean").unwrap().collect::<Vec<_>>(),
        [false]
    );
    assert_eq!(table.string_column("text").unwrap(), ["seed"]);
}

struct FailingReader {
    bytes: &'static [u8],
    position: usize,
    fail_at: usize,
}

impl Read for FailingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.fail_at {
            return Err(io::Error::other("injected read failure"));
        }
        let available = self.fail_at.min(self.bytes.len()) - self.position;
        let count = available.min(buffer.len());
        buffer[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}
