use std::io::{BufReader, Read};

pub const MAX_STREAM_RECORD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Float,
    Boolean,
    String,
}

#[derive(Debug, Clone, PartialEq)]
enum NormalizedValue {
    Integer(i128),
    Float(f64),
    Boolean(bool),
    String(String),
}

#[derive(Debug, Clone, PartialEq)]
struct NormalizedTable {
    rows: Vec<Vec<NormalizedValue>>,
}

#[derive(Debug, Clone)]
pub struct ResultOracle {
    table: NormalizedTable,
    canonical_digest: String,
}

impl ResultOracle {
    pub fn canonical_digest(&self) -> &str {
        &self.canonical_digest
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationSummary {
    pub verified_results: usize,
    pub canonical_digest: String,
}

pub fn result_oracle(
    csv: &str,
    columns: &[(&str, ColumnType)],
    engine: &str,
) -> Result<ResultOracle, String> {
    let table = normalize(csv, columns, engine)?;
    let canonical_digest = canonical_digest(&table, columns, 1);
    Ok(ResultOracle {
        table,
        canonical_digest,
    })
}

pub fn compare_oracles(
    rusthouse: &ResultOracle,
    clickhouse: &ResultOracle,
    columns: &[(&str, ColumnType)],
) -> Result<(), String> {
    compare_tables(
        &rusthouse.table,
        &clickhouse.table,
        columns,
        "RustHouse",
        "ClickHouse",
    )
}

#[cfg(test)]
pub fn compare_outputs(
    rusthouse_csv: &str,
    clickhouse_csv: &str,
    columns: &[(&str, ColumnType)],
) -> Result<(), String> {
    let rusthouse = result_oracle(rusthouse_csv, columns, "RustHouse")?;
    let clickhouse = result_oracle(clickhouse_csv, columns, "ClickHouse")?;
    compare_oracles(&rusthouse, &clickhouse, columns)
}

pub fn validate_repeated_outputs(
    reader: impl Read,
    oracle: &ResultOracle,
    columns: &[(&str, ColumnType)],
    engine: &str,
    expected_repetitions: usize,
) -> Result<ValidationSummary, String> {
    if expected_repetitions == 0 {
        return Err("result repetition count must be positive".to_owned());
    }

    let expected_header = columns.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    let mut records = CsvRecordReader::new(reader);
    let mut digest = CanonicalDigest::new(columns, expected_repetitions);

    for repetition in 0..expected_repetitions {
        let Some(mut header) = records
            .next_record()
            .map_err(|error| format!("{engine} CSV: {error}"))?
        else {
            return Err(format!(
                "{engine} result repetition count mismatch: expected {expected_repetitions}, got {repetition}"
            ));
        };
        if repetition > 0 && header.len() == 1 && header[0].is_empty() {
            let Some(next_header) = records
                .next_record()
                .map_err(|error| format!("{engine} CSV: {error}"))?
            else {
                return Err(format!(
                    "{engine} result repetition count mismatch: expected {expected_repetitions}, got {repetition}"
                ));
            };
            header = next_header;
        }
        if header.iter().map(String::as_str).collect::<Vec<_>>() != expected_header {
            return Err(format!(
                "{engine} repetition {} header mismatch: expected {expected_header:?}, got {header:?}",
                repetition + 1
            ));
        }

        digest.start_result(oracle.table.rows.len());
        for (row_index, expected_row) in oracle.table.rows.iter().enumerate() {
            let Some(row) = records
                .next_record()
                .map_err(|error| format!("{engine} CSV: {error}"))?
            else {
                return Err(format!(
                    "{engine} repetition {} ended after {row_index} of {} rows",
                    repetition + 1,
                    oracle.table.rows.len()
                ));
            };
            let actual_row = normalize_row(
                &row,
                columns,
                &format!("{engine} repetition {}", repetition + 1),
                row_index,
            )?;
            compare_rows(
                &actual_row,
                expected_row,
                columns,
                row_index,
                &format!("{engine} repetition {}", repetition + 1),
                "correctness oracle",
            )?;
            digest.add_row(&actual_row);
        }
    }

    if records
        .next_record()
        .map_err(|error| format!("{engine} CSV: {error}"))?
        .is_some()
    {
        return Err(format!(
            "{engine} result repetition count mismatch: expected {expected_repetitions}, got more than {expected_repetitions}"
        ));
    }

    Ok(ValidationSummary {
        verified_results: expected_repetitions,
        canonical_digest: digest.finish(),
    })
}

fn compare_tables(
    left: &NormalizedTable,
    right: &NormalizedTable,
    columns: &[(&str, ColumnType)],
    left_name: &str,
    right_name: &str,
) -> Result<(), String> {
    if left.rows.len() != right.rows.len() {
        return Err(format!(
            "row count mismatch: {left_name} returned {}, {right_name} returned {}",
            left.rows.len(),
            right.rows.len()
        ));
    }

    for (row_index, (left_row, right_row)) in left.rows.iter().zip(&right.rows).enumerate() {
        compare_rows(
            left_row, right_row, columns, row_index, left_name, right_name,
        )?;
    }
    Ok(())
}

fn compare_rows(
    left: &[NormalizedValue],
    right: &[NormalizedValue],
    columns: &[(&str, ColumnType)],
    row_index: usize,
    left_name: &str,
    right_name: &str,
) -> Result<(), String> {
    for (column_index, (left, right)) in left.iter().zip(right).enumerate() {
        if !values_equal(left, right) {
            return Err(format!(
                "result mismatch at row {}, column '{}': {left_name}={left:?}, {right_name}={right:?}",
                row_index + 1,
                columns[column_index].0
            ));
        }
    }
    Ok(())
}

fn normalize(
    csv: &str,
    columns: &[(&str, ColumnType)],
    engine: &str,
) -> Result<NormalizedTable, String> {
    let records = parse_csv(csv).map_err(|error| format!("{engine} CSV: {error}"))?;
    let (header, rows) = records
        .split_first()
        .ok_or_else(|| format!("{engine} returned no CSV header"))?;
    let expected_header = columns.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    if header.iter().map(String::as_str).collect::<Vec<_>>() != expected_header {
        return Err(format!(
            "{engine} header mismatch: expected {expected_header:?}, got {header:?}"
        ));
    }

    let normalized_rows = rows
        .iter()
        .enumerate()
        .map(|(row_index, row)| normalize_row(row, columns, engine, row_index))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(NormalizedTable {
        rows: normalized_rows,
    })
}

fn normalize_row(
    row: &[String],
    columns: &[(&str, ColumnType)],
    engine: &str,
    row_index: usize,
) -> Result<Vec<NormalizedValue>, String> {
    if row.len() != columns.len() {
        return Err(format!(
            "{engine} row {} has {} columns; expected {}",
            row_index + 1,
            row.len(),
            columns.len()
        ));
    }
    row.iter()
        .zip(columns)
        .enumerate()
        .map(|(column_index, (value, (_, column_type)))| {
            normalize_value(value, *column_type).map_err(|error| {
                format!(
                    "{engine} row {}, column '{}': {error}",
                    row_index + 1,
                    columns[column_index].0
                )
            })
        })
        .collect()
}

fn normalize_value(value: &str, column_type: ColumnType) -> Result<NormalizedValue, String> {
    match column_type {
        ColumnType::Integer => value
            .parse::<i128>()
            .map(NormalizedValue::Integer)
            .map_err(|_| format!("invalid integer {value:?}")),
        ColumnType::Float => {
            let parsed = value
                .parse::<f64>()
                .map_err(|_| format!("invalid float {value:?}"))?;
            if !parsed.is_finite() {
                return Err(format!("non-finite float {value:?}"));
            }
            Ok(NormalizedValue::Float(parsed))
        }
        ColumnType::Boolean => match value.to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(NormalizedValue::Boolean(true)),
            "false" | "0" => Ok(NormalizedValue::Boolean(false)),
            _ => Err(format!("invalid boolean {value:?}")),
        },
        ColumnType::String => Ok(NormalizedValue::String(value.to_owned())),
    }
}

fn values_equal(left: &NormalizedValue, right: &NormalizedValue) -> bool {
    match (left, right) {
        (NormalizedValue::Float(left), NormalizedValue::Float(right)) => {
            let scale = left.abs().max(right.abs()).max(1.0);
            (left - right).abs() <= 1e-9 * scale
        }
        _ => left == right,
    }
}

fn parse_csv(input: &str) -> Result<Vec<Vec<String>>, String> {
    let mut reader = CsvRecordReader::new(input.as_bytes());
    let mut records = Vec::new();
    while let Some(record) = reader.next_record()? {
        records.push(record);
    }
    Ok(records)
}

struct CsvRecordReader<R> {
    reader: BufReader<R>,
    pending: Option<u8>,
}

impl<R: Read> CsvRecordReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            pending: None,
        }
    }

    fn next_record(&mut self) -> Result<Option<Vec<String>>, String> {
        let mut record = Vec::new();
        let mut field = Vec::new();
        let mut record_bytes = 0_usize;
        let mut record_started = false;
        let mut field_started = false;
        let mut in_quotes = false;
        let mut closed_quote = false;

        loop {
            let Some(byte) = self.next_byte()? else {
                if in_quotes {
                    return Err("unterminated quoted field".to_owned());
                }
                if !record_started {
                    return Ok(None);
                }
                push_field(&mut record, &mut field)?;
                return Ok(Some(record));
            };
            record_started = true;
            record_bytes += 1;
            if record_bytes > MAX_STREAM_RECORD_BYTES {
                return Err(format!(
                    "CSV record exceeds the {MAX_STREAM_RECORD_BYTES}-byte streaming limit"
                ));
            }

            if in_quotes {
                if byte == b'"' {
                    in_quotes = false;
                    closed_quote = true;
                } else {
                    field.push(byte);
                }
                continue;
            }

            if closed_quote {
                match byte {
                    b'"' => {
                        field.push(b'"');
                        in_quotes = true;
                        closed_quote = false;
                    }
                    b',' => {
                        push_field(&mut record, &mut field)?;
                        field_started = false;
                        closed_quote = false;
                    }
                    b'\n' => {
                        push_field(&mut record, &mut field)?;
                        return Ok(Some(record));
                    }
                    b'\r' => {
                        self.consume_optional_line_feed()?;
                        push_field(&mut record, &mut field)?;
                        return Ok(Some(record));
                    }
                    _ => return Err("characters after a closing quote".to_owned()),
                }
                continue;
            }

            match byte {
                b'"' if !field_started => {
                    in_quotes = true;
                    field_started = true;
                }
                b'"' => return Err("quote in the middle of an unquoted field".to_owned()),
                b',' => {
                    push_field(&mut record, &mut field)?;
                    field_started = false;
                }
                b'\n' => {
                    push_field(&mut record, &mut field)?;
                    return Ok(Some(record));
                }
                b'\r' => {
                    self.consume_optional_line_feed()?;
                    push_field(&mut record, &mut field)?;
                    return Ok(Some(record));
                }
                value => {
                    field.push(value);
                    field_started = true;
                }
            }
        }
    }

    fn next_byte(&mut self) -> Result<Option<u8>, String> {
        if self.pending.is_some() {
            return Ok(self.pending.take());
        }
        let mut byte = [0_u8; 1];
        match self.reader.read(&mut byte) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(byte[0])),
            Err(error) => Err(format!("could not read output: {error}")),
        }
    }

    fn consume_optional_line_feed(&mut self) -> Result<(), String> {
        if let Some(byte) = self.next_byte()?
            && byte != b'\n'
        {
            self.pending = Some(byte);
        }
        Ok(())
    }
}

fn push_field(record: &mut Vec<String>, field: &mut Vec<u8>) -> Result<(), String> {
    let value = String::from_utf8(std::mem::take(field))
        .map_err(|error| format!("field was not UTF-8: {error}"))?;
    record.push(value);
    Ok(())
}

fn canonical_digest(
    table: &NormalizedTable,
    columns: &[(&str, ColumnType)],
    repetitions: usize,
) -> String {
    let mut digest = CanonicalDigest::new(columns, repetitions);
    for _ in 0..repetitions {
        digest.start_result(table.rows.len());
        for row in &table.rows {
            digest.add_row(row);
        }
    }
    digest.finish()
}

struct CanonicalDigest {
    hash: Sha256,
}

impl CanonicalDigest {
    fn new(columns: &[(&str, ColumnType)], repetitions: usize) -> Self {
        let mut value = Self {
            hash: Sha256::new(),
        };
        value.hash.update(b"rusthouse-canonical-results-v1\0");
        value.add_usize(repetitions);
        value.add_usize(columns.len());
        for (name, column_type) in columns {
            value.add_bytes(name.as_bytes());
            value.hash.update(&[match column_type {
                ColumnType::Integer => 1,
                ColumnType::Float => 2,
                ColumnType::Boolean => 3,
                ColumnType::String => 4,
            }]);
        }
        value
    }

    fn start_result(&mut self, row_count: usize) {
        self.hash.update(&[0xf0]);
        self.add_usize(row_count);
    }

    fn add_row(&mut self, row: &[NormalizedValue]) {
        self.hash.update(&[0xf1]);
        self.add_usize(row.len());
        for value in row {
            match value {
                NormalizedValue::Integer(value) => {
                    self.hash.update(&[1]);
                    self.hash.update(&value.to_be_bytes());
                }
                NormalizedValue::Float(value) => {
                    self.hash.update(&[2]);
                    self.hash.update(&value.to_bits().to_be_bytes());
                }
                NormalizedValue::Boolean(value) => {
                    self.hash.update(&[3, u8::from(*value)]);
                }
                NormalizedValue::String(value) => {
                    self.hash.update(&[4]);
                    self.add_bytes(value.as_bytes());
                }
            }
        }
    }

    fn add_bytes(&mut self, bytes: &[u8]) {
        self.add_usize(bytes.len());
        self.hash.update(bytes);
    }

    fn add_usize(&mut self, value: usize) {
        self.hash.update(&(value as u64).to_be_bytes());
    }

    fn finish(self) -> String {
        self.hash
            .finish()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    block_len: usize,
    message_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            block_len: 0,
            message_len: 0,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.message_len = self
            .message_len
            .checked_add(bytes.len() as u64)
            .expect("canonical result is too large to digest");
        for byte in bytes {
            self.block[self.block_len] = *byte;
            self.block_len += 1;
            if self.block_len == self.block.len() {
                self.compress();
            }
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let message_bits = self
            .message_len
            .checked_mul(8)
            .expect("canonical result is too large to digest");
        self.update(&[0x80]);
        while self.block_len != 56 {
            self.update(&[0]);
        }
        self.update(&message_bits.to_be_bytes());

        let mut output = [0_u8; 32];
        for (chunk, value) in output.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&value.to_be_bytes());
        }
        output
    }

    fn compress(&mut self) {
        const ROUND_CONSTANTS: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];

        let mut schedule = [0_u32; 64];
        for (index, chunk) in self.block.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes(chunk.try_into().expect("four-byte chunk"));
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temporary1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(ROUND_CONSTANTS[index])
                .wrapping_add(schedule[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary2 = sum0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary1);
            d = c;
            c = b;
            b = a;
            a = temporary1.wrapping_add(temporary2);
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
        self.block_len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_engine_specific_scalar_spellings_and_csv_escaping() {
        let columns = [
            ("n", ColumnType::Integer),
            ("mean", ColumnType::Float),
            ("enabled", ColumnType::Boolean),
            ("label", ColumnType::String),
        ];
        let rusthouse = "n,mean,enabled,label\n2,1.0,true,\"comma, and \"\"quote\"\"\"\n";
        let clickhouse = "n,mean,enabled,label\r\n2,1,1,\"comma, and \"\"quote\"\"\"\r\n";

        compare_outputs(rusthouse, clickhouse, &columns).expect("equivalent output");
    }

    #[test]
    fn float_comparison_allows_rendering_noise_but_not_real_differences() {
        let columns = [("mean", ColumnType::Float)];
        compare_outputs(
            "mean\n0.3333333333333333\n",
            "mean\n0.33333333333333331\n",
            &columns,
        )
        .expect("rendering-only difference");
        assert!(compare_outputs("mean\n1.0\n", "mean\n1.01\n", &columns).is_err());
    }

    #[test]
    fn rejects_malformed_or_mismatched_output() {
        let columns = [("value", ColumnType::String)];
        assert!(compare_outputs("value\nleft\n", "value\nright\n", &columns).is_err());
        assert!(compare_outputs("wrong\nleft\n", "value\nleft\n", &columns).is_err());
        assert!(compare_outputs("value\n\"unfinished\n", "value\nleft\n", &columns).is_err());
    }

    #[test]
    fn repeated_validation_requires_every_configured_result() {
        let columns = [("n", ColumnType::Integer), ("label", ColumnType::String)];
        let oracle =
            result_oracle("n,label\n1,first\n2,second\n", &columns, "oracle").expect("oracle");
        let repeated = "n,label\n1,first\n2,second\nn,label\n1,first\n2,second\n";

        let summary =
            validate_repeated_outputs(repeated.as_bytes(), &oracle, &columns, "engine", 2)
                .expect("all repetitions match");
        assert_eq!(summary.verified_results, 2);
        assert_eq!(summary.canonical_digest.len(), 64);

        let missing = "n,label\n1,first\n2,second\n";
        assert!(
            validate_repeated_outputs(missing.as_bytes(), &oracle, &columns, "engine", 2)
                .expect_err("missing repetition")
                .contains("expected 2, got 1")
        );

        let extra = format!("{repeated}n,label\n1,first\n2,second\n");
        assert!(
            validate_repeated_outputs(extra.as_bytes(), &oracle, &columns, "engine", 2)
                .expect_err("extra repetition")
                .contains("got more than 2")
        );
    }

    #[test]
    fn repeated_validation_accepts_a_single_empty_csv_result_separator() {
        let columns = [("n", ColumnType::Integer)];
        let oracle = result_oracle("n\n1\n", &columns, "oracle").expect("oracle");
        let rusthouse_style = "n\n1\n\nn\n1\n\nn\n1\n";

        let summary =
            validate_repeated_outputs(rusthouse_style.as_bytes(), &oracle, &columns, "engine", 3)
                .expect("empty result separators");
        assert_eq!(summary.verified_results, 3);
    }

    #[test]
    fn repeated_validation_rejects_reordered_rows() {
        let columns = [("n", ColumnType::Integer)];
        let oracle = result_oracle("n\n1\n2\n", &columns, "oracle").expect("oracle");
        let reordered = "n\n1\n2\nn\n2\n1\n";

        let error = validate_repeated_outputs(reordered.as_bytes(), &oracle, &columns, "engine", 2)
            .expect_err("reordered result");
        assert!(error.contains("repetition 2"));
        assert!(error.contains("row 1"));
    }

    #[test]
    fn repeated_validation_rejects_one_corrupted_repetition() {
        let columns = [
            ("mean", ColumnType::Float),
            ("enabled", ColumnType::Boolean),
        ];
        let oracle = result_oracle("mean,enabled\n1.0,true\n", &columns, "oracle").expect("oracle");
        let selectively_corrupted =
            "mean,enabled\n1.0000000001,1\nmean,enabled\n1.25,1\nmean,enabled\n1.0,true\n";

        let error = validate_repeated_outputs(
            selectively_corrupted.as_bytes(),
            &oracle,
            &columns,
            "engine",
            3,
        )
        .expect_err("corrupted repetition");
        assert!(error.contains("repetition 2"));
        assert!(error.contains("mean"));
    }

    #[test]
    fn streaming_parser_handles_embedded_newlines_and_enforces_its_bound() {
        let columns = [("value", ColumnType::String)];
        let oracle =
            result_oracle("value\n\"first\nsecond\"\n", &columns, "oracle").expect("oracle");
        validate_repeated_outputs(
            "value\n\"first\nsecond\"\n".as_bytes(),
            &oracle,
            &columns,
            "engine",
            1,
        )
        .expect("embedded newline");

        let oversized = format!("value\n{}\n", "x".repeat(MAX_STREAM_RECORD_BYTES + 1));
        assert!(
            validate_repeated_outputs(oversized.as_bytes(), &oracle, &columns, "engine", 1)
                .expect_err("oversized record")
                .contains("streaming limit")
        );
    }

    #[test]
    fn sha256_matches_the_standard_test_vector() {
        let mut digest = Sha256::new();
        digest.update(b"abc");
        let rendered = digest
            .finish()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            rendered,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
