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
    compare_output_sequences(rusthouse_csv, clickhouse_csv, columns, 1)
}

pub fn compare_output_sequences(
    rusthouse_csv: &str,
    clickhouse_csv: &str,
    columns: &[(&str, ColumnType)],
    expected_results: usize,
) -> Result<(), String> {
    if expected_results == 0 {
        return Err("cannot compare an empty query sequence".to_owned());
    }
    let rusthouse = normalize_sequence(rusthouse_csv, columns, "RustHouse", expected_results)?;
    let clickhouse = normalize_sequence(clickhouse_csv, columns, "ClickHouse", expected_results)?;

    for (result_index, (rusthouse, clickhouse)) in rusthouse.iter().zip(&clickhouse).enumerate() {
        if rusthouse.rows.len() != clickhouse.rows.len() {
            return Err(format!(
                "row count mismatch in query result {}: RustHouse returned {}, ClickHouse returned {}",
                result_index + 1,
                rusthouse.rows.len(),
                clickhouse.rows.len()
            ));
        }

        for (row_index, (left, right)) in rusthouse.rows.iter().zip(&clickhouse.rows).enumerate() {
            for (column_index, (left, right)) in left.iter().zip(right).enumerate() {
                if !values_equal(left, right) {
                    return Err(format!(
                        "result mismatch in query result {}, row {}, column '{}': RustHouse={left:?}, ClickHouse={right:?}",
                        result_index + 1,
                        row_index + 1,
                        columns[column_index].0
                    ));
                }
            }
        }
    }
    Ok(())
}

fn normalize_sequence(
    csv: &str,
    columns: &[(&str, ColumnType)],
    engine: &str,
    expected_results: usize,
) -> Result<Vec<NormalizedTable>, String> {
    let records = parse_csv(csv).map_err(|error| format!("{engine} CSV: {error}"))?;
    let expected_header = columns.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    let mut results = Vec::new();
    let mut current_rows: Option<Vec<Vec<NormalizedValue>>> = None;

    for record in records {
        // RustHouse puts one blank line between CSV result sets. Benchmark
        // workloads have at least two columns, so this cannot hide a data row.
        if columns.len() > 1 && record.len() == 1 && record[0].is_empty() {
            continue;
        }
        let is_header = record.iter().map(String::as_str).collect::<Vec<_>>() == expected_header;
        if is_header {
            if let Some(rows) = current_rows.replace(Vec::new()) {
                results.push(NormalizedTable { rows });
            }
            continue;
        }
        let result_index = results.len() + 1;
        let Some(rows) = current_rows.as_mut() else {
            return Err(format!(
                "{engine} header mismatch: expected {expected_header:?}, got {record:?}"
            ));
        };
        if record.len() != columns.len() {
            return Err(format!(
                "{engine} query result {}, row {} has {} columns; expected {}",
                result_index,
                rows.len() + 1,
                record.len(),
                columns.len()
            ));
        }
        let normalized = record
            .iter()
            .zip(columns)
            .enumerate()
            .map(|(column_index, (value, (_, column_type)))| {
                normalize_value(value, *column_type).map_err(|error| {
                    format!(
                        "{engine} query result {}, row {}, column '{}': {error}",
                        result_index,
                        rows.len() + 1,
                        columns[column_index].0
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        rows.push(normalized);
    }
    if let Some(rows) = current_rows {
        results.push(NormalizedTable { rows });
    }
    if results.len() != expected_results {
        return Err(format!(
            "{engine} returned {} query results; expected {expected_results}",
            results.len()
        ));
    }
    Ok(results)
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
    fn compares_every_result_in_an_amplified_sequence() {
        let columns = [("n", ColumnType::Integer)];
        let rusthouse = "n\n1\nn\n2\nn\n3\n";
        let clickhouse = "n\r\n1\r\nn\r\n2\r\nn\r\n3\r\n";
        compare_output_sequences(rusthouse, clickhouse, &columns, 3).expect("all results match");

        let mismatch = compare_output_sequences(rusthouse, "n\n1\nn\n9\nn\n3\n", &columns, 3)
            .expect_err("middle result mismatch must fail");
        assert!(mismatch.contains("query result 2"));
        assert!(compare_output_sequences(rusthouse, clickhouse, &columns, 2).is_err());
    }

    #[test]
    fn accepts_rusthouse_blank_lines_between_multicolumn_results() {
        let columns = [("n", ColumnType::Integer), ("label", ColumnType::String)];
        let rusthouse = "n,label\n1,a\n\nn,label\n2,b\n";
        let clickhouse = "\"n\",\"label\"\n1,a\n\"n\",\"label\"\n2,b\n";
        compare_output_sequences(rusthouse, clickhouse, &columns, 2)
            .expect("separator-only blank lines");
    }
}
