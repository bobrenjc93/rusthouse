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

pub fn compare_outputs(
    rusthouse_csv: &str,
    clickhouse_csv: &str,
    columns: &[(&str, ColumnType)],
) -> Result<(), String> {
    let rusthouse = normalize(rusthouse_csv, columns, "RustHouse")?;
    let clickhouse = normalize(clickhouse_csv, columns, "ClickHouse")?;

    if rusthouse.rows.len() != clickhouse.rows.len() {
        return Err(format!(
            "row count mismatch: RustHouse returned {}, ClickHouse returned {}",
            rusthouse.rows.len(),
            clickhouse.rows.len()
        ));
    }

    for (row_index, (left, right)) in rusthouse.rows.iter().zip(&clickhouse.rows).enumerate() {
        for (column_index, (left, right)) in left.iter().zip(right).enumerate() {
            if !values_equal(left, right) {
                return Err(format!(
                    "result mismatch at row {}, column '{}': RustHouse={left:?}, ClickHouse={right:?}",
                    row_index + 1,
                    columns[column_index].0
                ));
            }
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

    let mut normalized_rows = Vec::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        if row.len() != columns.len() {
            return Err(format!(
                "{engine} row {} has {} columns; expected {}",
                row_index + 1,
                row.len(),
                columns.len()
            ));
        }
        let normalized = row
            .iter()
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
            .collect::<Result<Vec<_>, _>>()?;
        normalized_rows.push(normalized);
    }

    Ok(NormalizedTable {
        rows: normalized_rows,
    })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CsvState {
    FieldStart,
    Unquoted,
    Quoted,
    AfterQuote,
}

fn parse_csv(input: &str) -> Result<Vec<Vec<String>>, String> {
    let characters = input.chars().collect::<Vec<_>>();
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut index = 0;
    let mut state = CsvState::FieldStart;

    while index < characters.len() {
        let character = characters[index];
        match state {
            CsvState::FieldStart => match character {
                '"' => state = CsvState::Quoted,
                ',' => record.push(String::new()),
                '\n' => finish_record(&mut records, &mut record, String::new()),
                '\r' => {
                    consume_lf(&characters, &mut index);
                    finish_record(&mut records, &mut record, String::new());
                }
                value => {
                    field.push(value);
                    state = CsvState::Unquoted;
                }
            },
            CsvState::Unquoted => match character {
                '"' => return Err("quote in the middle of an unquoted field".to_owned()),
                ',' => {
                    record.push(std::mem::take(&mut field));
                    state = CsvState::FieldStart;
                }
                '\n' => {
                    finish_record(&mut records, &mut record, std::mem::take(&mut field));
                    state = CsvState::FieldStart;
                }
                '\r' => {
                    consume_lf(&characters, &mut index);
                    finish_record(&mut records, &mut record, std::mem::take(&mut field));
                    state = CsvState::FieldStart;
                }
                value => field.push(value),
            },
            CsvState::Quoted => {
                if character == '"' {
                    if characters.get(index + 1) == Some(&'"') {
                        field.push('"');
                        index += 1;
                    } else {
                        state = CsvState::AfterQuote;
                    }
                } else {
                    field.push(character);
                }
            }
            CsvState::AfterQuote => match character {
                ',' => {
                    record.push(std::mem::take(&mut field));
                    state = CsvState::FieldStart;
                }
                '\n' => {
                    finish_record(&mut records, &mut record, std::mem::take(&mut field));
                    state = CsvState::FieldStart;
                }
                '\r' => {
                    consume_lf(&characters, &mut index);
                    finish_record(&mut records, &mut record, std::mem::take(&mut field));
                    state = CsvState::FieldStart;
                }
                _ => return Err("character after closing quote".to_owned()),
            },
        }
        index += 1;
    }

    match state {
        CsvState::Quoted => return Err("unterminated quoted field".to_owned()),
        CsvState::Unquoted | CsvState::AfterQuote => {
            finish_record(&mut records, &mut record, field);
        }
        CsvState::FieldStart if !record.is_empty() => {
            finish_record(&mut records, &mut record, String::new());
        }
        CsvState::FieldStart => {}
    }
    Ok(records)
}

fn consume_lf(characters: &[char], index: &mut usize) {
    if characters.get(*index + 1) == Some(&'\n') {
        *index += 1;
    }
}

fn finish_record(records: &mut Vec<Vec<String>>, record: &mut Vec<String>, field: String) {
    record.push(field);
    records.push(std::mem::take(record));
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusthouse::engine::{QueryResult, ResultColumn};
    use rusthouse::format::{self, OutputFormat};
    use rusthouse::{DataType, Value};

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
    fn renderer_csv_round_trips_bounded_typed_values() {
        let columns = [
            ("integer", ColumnType::Integer),
            ("float", ColumnType::Float),
            ("boolean", ColumnType::Boolean),
            ("string", ColumnType::String),
        ];
        let strings = [
            String::new(),
            "plain".to_owned(),
            "comma,inside".to_owned(),
            "double \"quote\"".to_owned(),
            "line one\nline two".to_owned(),
            "carriage\rreturn".to_owned(),
            "unicode \u{2603}".to_owned(),
        ];

        for case in 0..256_i64 {
            let integer = case - 128;
            let float = (case - 100) as f64 / 8.0;
            let boolean = case % 2 == 0;
            let string = format!("{}-{case}", strings[case as usize % strings.len()]);
            let result = QueryResult {
                columns: vec![
                    ResultColumn {
                        name: "integer".to_owned(),
                        data_type: DataType::Int64,
                    },
                    ResultColumn {
                        name: "float".to_owned(),
                        data_type: DataType::Float64,
                    },
                    ResultColumn {
                        name: "boolean".to_owned(),
                        data_type: DataType::Bool,
                    },
                    ResultColumn {
                        name: "string".to_owned(),
                        data_type: DataType::String,
                    },
                ],
                rows: vec![vec![
                    Value::Int64(integer),
                    Value::Float64(float),
                    Value::Bool(boolean),
                    Value::String(string.clone()),
                ]],
            };

            let csv = format::render(&result, OutputFormat::Csv);
            let normalized = normalize(&csv, &columns, "round trip").expect("valid CSV");
            assert_eq!(
                normalized.rows,
                [vec![
                    NormalizedValue::Integer(i128::from(integer)),
                    NormalizedValue::Float(float),
                    NormalizedValue::Boolean(boolean),
                    NormalizedValue::String(string),
                ]],
                "case {case}: {csv:?}"
            );
        }
    }

    #[test]
    fn malformed_quoted_fields_are_always_rejected() {
        let columns = [("value", ColumnType::String)];
        for malformed in [
            "value\n\"closed\"trailing\n",
            "value\nun\"quoted\n",
            "value\n\"closed\" \n",
            "value\n\"unterminated",
            "value\n\"\"\"",
        ] {
            assert!(
                compare_outputs(malformed, "value\nvalid\n", &columns).is_err(),
                "accepted malformed CSV: {malformed:?}"
            );
        }
    }

    #[test]
    fn bounded_arbitrary_csv_is_panic_free() {
        let columns = [
            ("i", ColumnType::Integer),
            ("f", ColumnType::Float),
            ("b", ColumnType::Boolean),
            ("s", ColumnType::String),
        ];
        let alphabet = [
            '\0',
            '\n',
            '\r',
            ',',
            '"',
            '-',
            '+',
            '.',
            '0',
            '9',
            'e',
            'N',
            'a',
            't',
            'f',
            '\u{80}',
            '\u{2028}',
            '\u{10ffff}',
        ];
        let mut state = 0xbb67_ae85_84ca_a73b_u64;

        for case in 0..512 {
            let mut input = String::new();
            for _ in 0..case % 129 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                input.push(alphabet[state as usize % alphabet.len()]);
            }
            let _ = compare_outputs(&input, &input, &columns);
        }
    }
}
