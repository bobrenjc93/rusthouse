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

pub fn validate_amplified_output(
    single_csv: &str,
    amplified_csv: &str,
    columns: &[(&str, ColumnType)],
    engine: &str,
    expected_repetitions: usize,
) -> Result<usize, String> {
    if expected_repetitions == 0 {
        return Err("amplified validation requires at least one repetition".to_owned());
    }

    let expected = normalize(single_csv, columns, engine)?;
    let records = parse_csv(amplified_csv).map_err(|error| format!("{engine} CSV: {error}"))?;
    let expected_header = columns
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect::<Vec<_>>();
    let records_per_result = expected
        .rows
        .len()
        .checked_add(1)
        .ok_or_else(|| "amplified validation result is too large".to_owned())?;
    let mut record_index = 0;

    for repetition in 0..expected_repetitions {
        if repetition > 0 {
            while records
                .get(record_index)
                .is_some_and(|record| is_blank_record(record))
            {
                record_index += 1;
            }
        }

        let end = record_index
            .checked_add(records_per_result)
            .ok_or_else(|| "amplified validation result is too large".to_owned())?;
        let result_records = records.get(record_index..end).ok_or_else(|| {
            format!(
                "{engine} amplified output is missing repetition {} of {expected_repetitions}",
                repetition + 1
            )
        })?;
        if result_records.first() != Some(&expected_header) {
            return Err(format!(
                "{engine} amplified output expected header {expected_header:?} for repetition {}, got {:?}",
                repetition + 1,
                result_records.first()
            ));
        }

        let actual = normalize_records(result_records, columns, engine).map_err(|error| {
            format!(
                "{engine} amplified repetition {} of {expected_repetitions}: {error}",
                repetition + 1
            )
        })?;
        compare_normalized(&expected, &actual, columns).map_err(|error| {
            format!(
                "{engine} amplified repetition {} of {expected_repetitions}: {error}",
                repetition + 1
            )
        })?;
        record_index = end;
    }

    while records
        .get(record_index)
        .is_some_and(|record| is_blank_record(record))
    {
        record_index += 1;
    }
    if record_index != records.len() {
        return Err(format!(
            "{engine} amplified output has extra output after {expected_repetitions} repetitions"
        ));
    }

    Ok(expected_repetitions)
}

fn normalize(
    csv: &str,
    columns: &[(&str, ColumnType)],
    engine: &str,
) -> Result<NormalizedTable, String> {
    let records = parse_csv(csv).map_err(|error| format!("{engine} CSV: {error}"))?;
    normalize_records(&records, columns, engine)
}

fn normalize_records(
    records: &[Vec<String>],
    columns: &[(&str, ColumnType)],
    engine: &str,
) -> Result<NormalizedTable, String> {
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

fn compare_normalized(
    expected: &NormalizedTable,
    actual: &NormalizedTable,
    columns: &[(&str, ColumnType)],
) -> Result<(), String> {
    if expected.rows.len() != actual.rows.len() {
        return Err(format!(
            "row count mismatch: expected {}, got {}",
            expected.rows.len(),
            actual.rows.len()
        ));
    }

    for (row_index, (expected_row, actual_row)) in
        expected.rows.iter().zip(&actual.rows).enumerate()
    {
        for (column_index, (expected_value, actual_value)) in
            expected_row.iter().zip(actual_row).enumerate()
        {
            if !values_equal(expected_value, actual_value) {
                return Err(format!(
                    "result mismatch at row {}, column '{}': expected={expected_value:?}, got={actual_value:?}",
                    row_index + 1,
                    columns[column_index].0
                ));
            }
        }
    }
    Ok(())
}

fn is_blank_record(record: &[String]) -> bool {
    record.len() == 1 && record[0].is_empty()
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
    let characters = input.chars().collect::<Vec<_>>();
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut index = 0;
    let mut in_quotes = false;
    let mut field_started = false;

    while index < characters.len() {
        let character = characters[index];
        if in_quotes {
            if character == '"' {
                if characters.get(index + 1) == Some(&'"') {
                    field.push('"');
                    index += 1;
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(character);
            }
        } else {
            match character {
                '"' if !field_started => {
                    in_quotes = true;
                    field_started = true;
                }
                '"' => return Err("quote in the middle of an unquoted field".to_owned()),
                ',' => {
                    record.push(std::mem::take(&mut field));
                    field_started = false;
                }
                '\n' => {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                    field_started = false;
                }
                '\r' => {
                    if characters.get(index + 1) == Some(&'\n') {
                        index += 1;
                    }
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                    field_started = false;
                }
                value => {
                    field.push(value);
                    field_started = true;
                }
            }
        }
        index += 1;
    }

    if in_quotes {
        return Err("unterminated quoted field".to_owned());
    }
    if field_started || !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    Ok(records)
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
    fn validates_every_result_in_an_amplified_csv_stream() {
        let columns = [("n", ColumnType::Integer), ("label", ColumnType::String)];
        let single = "n,label\n1,first\n2,second\n";
        let amplified = concat!(
            "n,label\n1,first\n2,second\n",
            "\n",
            "n,label\r\n1,first\r\n2,second\r\n",
            "n,label\n1,first\n2,second\n"
        );

        assert_eq!(
            validate_amplified_output(single, amplified, &columns, "fake", 3)
                .expect("all repetitions match"),
            3
        );
    }

    #[test]
    fn amplified_validation_rejects_missing_reordered_and_extra_output() {
        let columns = [("n", ColumnType::Integer)];
        let single = "n\n1\n2\n";
        let missing = "n\n1\n2\nn\n1\n2\n";
        let reordered = "n\n1\n2\nn\n2\n1\nn\n1\n2\n";
        let extra = "n\n1\n2\nn\n1\n2\nn\n1\n2\nn\n1\n2\n";

        assert!(validate_amplified_output(single, missing, &columns, "fake", 3).is_err());
        assert!(validate_amplified_output(single, reordered, &columns, "fake", 3).is_err());
        assert!(validate_amplified_output(single, extra, &columns, "fake", 3).is_err());
    }
}
