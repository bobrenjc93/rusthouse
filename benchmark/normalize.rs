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

#[cfg(test)]
pub fn compare_outputs(
    rusthouse_csv: &str,
    clickhouse_csv: &str,
    columns: &[(&str, ColumnType)],
) -> Result<(), String> {
    compare_output_batches(rusthouse_csv, clickhouse_csv, columns, 1)
}

pub fn compare_output_batches(
    rusthouse_csv: &str,
    clickhouse_csv: &str,
    columns: &[(&str, ColumnType)],
    expected_results: usize,
) -> Result<(), String> {
    if expected_results == 0 {
        return Err("expected result count must be positive".to_owned());
    }
    let rusthouse = normalize_batch(rusthouse_csv, columns, "RustHouse", expected_results)?;
    let clickhouse = normalize_batch(clickhouse_csv, columns, "ClickHouse", expected_results)?;

    for (result_index, (left, right)) in rusthouse.iter().zip(&clickhouse).enumerate() {
        compare_tables(left, right, columns, result_index)?;
    }
    Ok(())
}

fn compare_tables(
    rusthouse: &NormalizedTable,
    clickhouse: &NormalizedTable,
    columns: &[(&str, ColumnType)],
    result_index: usize,
) -> Result<(), String> {
    if rusthouse.rows.len() != clickhouse.rows.len() {
        return Err(format!(
            "result {} row count mismatch: RustHouse returned {}, ClickHouse returned {}",
            result_index + 1,
            rusthouse.rows.len(),
            clickhouse.rows.len()
        ));
    }

    for (row_index, (left, right)) in rusthouse.rows.iter().zip(&clickhouse.rows).enumerate() {
        for (column_index, (left, right)) in left.iter().zip(right).enumerate() {
            if !values_equal(left, right) {
                return Err(format!(
                    "result mismatch in variant {}, row {}, column '{}': RustHouse={left:?}, ClickHouse={right:?}",
                    result_index + 1,
                    row_index + 1,
                    columns[column_index].0
                ));
            }
        }
    }
    Ok(())
}

fn normalize_batch(
    csv: &str,
    columns: &[(&str, ColumnType)],
    engine: &str,
    expected_results: usize,
) -> Result<Vec<NormalizedTable>, String> {
    let records = parse_csv(csv).map_err(|error| format!("{engine} CSV: {error}"))?;
    let expected_header = columns.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    let mut result_records = Vec::<Vec<Vec<String>>>::new();

    for record in records {
        let is_header = record
            .iter()
            .map(String::as_str)
            .eq(expected_header.iter().copied());
        if is_header {
            if let Some(rows) = result_records.last_mut()
                && rows.last().is_some_and(|row| row == &[String::new()])
            {
                rows.pop();
            }
            result_records.push(Vec::new());
        } else if let Some(rows) = result_records.last_mut() {
            rows.push(record);
        } else {
            return Err(format!(
                "{engine} header mismatch: expected {expected_header:?}, got {record:?}"
            ));
        }
    }

    if result_records.len() != expected_results {
        return Err(format!(
            "{engine} result count mismatch: expected {expected_results}, got {} (missing, extra, or malformed output)",
            result_records.len()
        ));
    }

    result_records
        .iter()
        .enumerate()
        .map(|(result_index, rows)| normalize_rows(rows, columns, engine, result_index))
        .collect()
}

fn normalize_rows(
    rows: &[Vec<String>],
    columns: &[(&str, ColumnType)],
    engine: &str,
    result_index: usize,
) -> Result<NormalizedTable, String> {
    let mut normalized_rows = Vec::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        if row.len() != columns.len() {
            return Err(format!(
                "{engine} result {}, row {} has {} columns; expected {}",
                result_index + 1,
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
                        "{engine} result {}, row {}, column '{}': {error}",
                        result_index + 1,
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
    fn compares_every_result_set_in_order() {
        let columns = [("value", ColumnType::Integer)];
        let rusthouse = "value\n1\n\nvalue\n2\n\nvalue\n3\n";
        let clickhouse = "value\n1\nvalue\n2\nvalue\n3\n";

        compare_output_batches(rusthouse, clickhouse, &columns, 3).expect("ordered batch");

        let reordered = "value\n1\nvalue\n3\nvalue\n2\n";
        let error = compare_output_batches(rusthouse, reordered, &columns, 3)
            .expect_err("reordered output must fail");
        assert!(error.contains("variant 2"));
    }

    #[test]
    fn rejects_missing_and_extra_result_sets() {
        let columns = [("value", ColumnType::Integer)];
        let expected = "value\n1\nvalue\n2\n";
        let missing = compare_output_batches(expected, "value\n1\n", &columns, 2)
            .expect_err("missing output must fail");
        assert!(missing.contains("result count mismatch"));

        let extra = compare_output_batches(expected, "value\n1\nvalue\n2\nvalue\n3\n", &columns, 2)
            .expect_err("extra output must fail");
        assert!(extra.contains("result count mismatch"));
    }
}
