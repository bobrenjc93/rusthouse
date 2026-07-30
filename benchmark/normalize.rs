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

    compare_tables(
        &rusthouse,
        &clickhouse,
        columns,
        "RustHouse",
        "ClickHouse",
        "",
    )
}

pub fn compare_repeated_outputs(
    rusthouse_csv: &str,
    clickhouse_csv: &str,
    columns: &[(&str, ColumnType)],
    expected_repetitions: usize,
) -> Result<(), String> {
    if expected_repetitions < 2 {
        return Err("amplified correctness requires at least two repetitions".to_owned());
    }
    let rusthouse = normalize_repeated(rusthouse_csv, columns, "RustHouse")?;
    let clickhouse = normalize_repeated(clickhouse_csv, columns, "ClickHouse")?;
    if rusthouse.len() != expected_repetitions || clickhouse.len() != expected_repetitions {
        return Err(format!(
            "amplified output count mismatch: expected {expected_repetitions}, RustHouse returned {}, ClickHouse returned {}",
            rusthouse.len(),
            clickhouse.len()
        ));
    }

    for index in 0..expected_repetitions {
        let context = format!(" in amplified repetition {}", index + 1);
        compare_tables(
            &rusthouse[index],
            &clickhouse[index],
            columns,
            "RustHouse",
            "ClickHouse",
            &context,
        )?;
        if index > 0 {
            compare_tables(
                &rusthouse[0],
                &rusthouse[index],
                columns,
                "RustHouse repetition 1",
                &format!("RustHouse repetition {}", index + 1),
                " in amplified output",
            )?;
            compare_tables(
                &clickhouse[0],
                &clickhouse[index],
                columns,
                "ClickHouse repetition 1",
                &format!("ClickHouse repetition {}", index + 1),
                " in amplified output",
            )?;
        }
    }
    Ok(())
}

fn compare_tables(
    rusthouse: &NormalizedTable,
    clickhouse: &NormalizedTable,
    columns: &[(&str, ColumnType)],
    left_label: &str,
    right_label: &str,
    context: &str,
) -> Result<(), String> {
    if rusthouse.rows.len() != clickhouse.rows.len() {
        return Err(format!(
            "row count mismatch{context}: {left_label} returned {}, {right_label} returned {}",
            rusthouse.rows.len(),
            clickhouse.rows.len()
        ));
    }

    for (row_index, (left, right)) in rusthouse.rows.iter().zip(&clickhouse.rows).enumerate() {
        for (column_index, (left, right)) in left.iter().zip(right).enumerate() {
            if !values_equal(left, right) {
                return Err(format!(
                    "result mismatch{context} at row {}, column '{}': {left_label}={left:?}, {right_label}={right:?}",
                    row_index + 1,
                    columns[column_index].0
                ));
            }
        }
    }
    Ok(())
}

fn normalize_repeated(
    csv: &str,
    columns: &[(&str, ColumnType)],
    engine: &str,
) -> Result<Vec<NormalizedTable>, String> {
    let records = parse_csv(csv).map_err(|error| format!("{engine} CSV: {error}"))?;
    let expected_header = columns.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    let mut tables = Vec::new();
    let mut rows = None::<Vec<Vec<String>>>;
    let mut separator_pending = false;

    for record in records {
        let is_header = record
            .iter()
            .map(String::as_str)
            .eq(expected_header.iter().copied());
        let is_blank = record.len() == 1 && record[0].is_empty();

        if is_header {
            if let Some(previous_rows) = rows.replace(Vec::new()) {
                tables.push(normalize_rows(&previous_rows, columns, engine)?);
            }
            separator_pending = false;
        } else if is_blank {
            if rows.is_none() || separator_pending {
                return Err(format!(
                    "{engine} amplified output contained an unexpected blank record"
                ));
            }
            separator_pending = true;
        } else if let Some(rows) = &mut rows {
            if separator_pending {
                return Err(format!(
                    "{engine} amplified output blank separator was not followed by the expected header"
                ));
            }
            rows.push(record);
        } else {
            return Err(format!(
                "{engine} amplified output did not start with the expected header"
            ));
        }
    }
    if separator_pending {
        return Err(format!(
            "{engine} amplified output ended after a blank separator"
        ));
    }
    if let Some(rows) = rows {
        tables.push(normalize_rows(&rows, columns, engine)?);
    }
    if tables.is_empty() {
        return Err(format!("{engine} returned no amplified CSV results"));
    }
    Ok(tables)
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

    normalize_rows(rows, columns, engine)
}

fn normalize_rows(
    rows: &[Vec<String>],
    columns: &[(&str, ColumnType)],
    engine: &str,
) -> Result<NormalizedTable, String> {
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
    fn amplified_output_validates_count_cross_engine_parity_and_repeatability() {
        let columns = [("n", ColumnType::Integer), ("mean", ColumnType::Float)];
        let rusthouse = "n,mean\n1,0.3333333333333333\n\nn,mean\n1,0.3333333333333333\n";
        let clickhouse = "n,mean\r\n1,0.33333333333333331\r\nn,mean\r\n1,0.33333333333333331\r\n";
        compare_repeated_outputs(rusthouse, clickhouse, &columns, 2)
            .expect("equivalent repeated output");

        assert!(compare_repeated_outputs(rusthouse, clickhouse, &columns, 3).is_err());
        assert!(
            compare_repeated_outputs(
                rusthouse,
                "n,mean\n1,0.3333333333333333\nn,mean\n2,0.3333333333333333\n",
                &columns,
                2
            )
            .is_err()
        );
    }

    #[test]
    fn amplified_output_accepts_captured_rusthouse_cli_separator() {
        let columns = [("n", ColumnType::Integer)];
        let rusthouse_cli = "n\n1\n\nn\n1\n";
        let clickhouse_cli = "\"n\"\n1\n\"n\"\n1\n";

        compare_repeated_outputs(rusthouse_cli, clickhouse_cli, &columns, 2)
            .expect("captured CLI outputs should match");
    }

    #[test]
    fn amplified_output_rejects_blank_records_outside_result_boundaries() {
        let columns = [("n", ColumnType::Integer)];
        let clickhouse = "n\n1\nn\n1\n";

        for malformed in [
            "\nn\n1\nn\n1\n",
            "n\n1\n\n2\nn\n1\n",
            "n\n1\n\n\nn\n1\n",
            "n\n1\nn\n1\n\n",
        ] {
            assert!(compare_repeated_outputs(malformed, clickhouse, &columns, 2).is_err());
        }
    }
}
