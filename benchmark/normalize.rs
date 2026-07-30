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

    compare_tables(&rusthouse, &clickhouse, columns, "RustHouse", "ClickHouse")
}

pub fn compare_repeated_outputs(
    rusthouse_expected_csv: &str,
    clickhouse_expected_csv: &str,
    rusthouse_repeated_csv: &str,
    clickhouse_repeated_csv: &str,
    columns: &[(&str, ColumnType)],
    expected_repetitions: usize,
) -> Result<(), String> {
    if expected_repetitions == 0 {
        return Err("expected repetition count must be positive".to_owned());
    }

    let rusthouse_expected = normalize(rusthouse_expected_csv, columns, "RustHouse expected")?;
    let clickhouse_expected = normalize(clickhouse_expected_csv, columns, "ClickHouse expected")?;
    compare_tables(
        &rusthouse_expected,
        &clickhouse_expected,
        columns,
        "RustHouse expected",
        "ClickHouse expected",
    )?;

    let rusthouse_repetitions = normalize_repeated(
        rusthouse_repeated_csv,
        columns,
        "RustHouse",
        rusthouse_expected.rows.len(),
    )?;
    let clickhouse_repetitions = normalize_repeated(
        clickhouse_repeated_csv,
        columns,
        "ClickHouse",
        clickhouse_expected.rows.len(),
    )?;
    require_repetition_count(
        "RustHouse",
        rusthouse_repetitions.len(),
        expected_repetitions,
    )?;
    require_repetition_count(
        "ClickHouse",
        clickhouse_repetitions.len(),
        expected_repetitions,
    )?;

    for (index, (rusthouse, clickhouse)) in rusthouse_repetitions
        .iter()
        .zip(&clickhouse_repetitions)
        .enumerate()
    {
        let repetition = index + 1;
        compare_tables(
            rusthouse,
            &rusthouse_expected,
            columns,
            &format!("RustHouse repetition {repetition}"),
            "RustHouse unamplified expected result",
        )?;
        compare_tables(
            clickhouse,
            &clickhouse_expected,
            columns,
            &format!("ClickHouse repetition {repetition}"),
            "ClickHouse unamplified expected result",
        )?;
        compare_tables(
            rusthouse,
            clickhouse,
            columns,
            &format!("RustHouse repetition {repetition}"),
            &format!("ClickHouse repetition {repetition}"),
        )?;
    }
    Ok(())
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
        for (column_index, (left_value, right_value)) in left_row.iter().zip(right_row).enumerate()
        {
            if !values_equal(left_value, right_value) {
                return Err(format!(
                    "result mismatch at row {}, column '{}': {left_name}={left_value:?}, {right_name}={right_value:?}",
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
    normalize_records(header, rows, columns, engine)
}

fn normalize_repeated(
    csv: &str,
    columns: &[(&str, ColumnType)],
    engine: &str,
    expected_rows: usize,
) -> Result<Vec<NormalizedTable>, String> {
    let records = parse_csv(csv).map_err(|error| format!("{engine} repeated CSV: {error}"))?;
    let mut documents = Vec::new();
    let mut position = 0;

    while position < records.len() {
        let document_number = documents.len() + 1;
        let header = &records[position];
        position += 1;
        let remaining = records.len() - position;
        if remaining < expected_rows {
            return Err(format!(
                "{engine} CSV result document {document_number} has only {remaining} rows available; expected {expected_rows}"
            ));
        }
        let rows = &records[position..position + expected_rows];
        let document = normalize_records(
            header,
            rows,
            columns,
            &format!("{engine} CSV result document {document_number}"),
        )?;
        documents.push(document);
        position += expected_rows;

        if position < records.len() && is_blank_record(&records[position]) {
            position += 1;
            if position == records.len() {
                return Err(format!("{engine} repeated CSV has a trailing blank record"));
            }
        }
    }

    Ok(documents)
}

fn is_blank_record(record: &[String]) -> bool {
    record.len() == 1 && record[0].is_empty()
}

fn require_repetition_count(engine: &str, actual: usize, expected: usize) -> Result<(), String> {
    if actual != expected {
        return Err(format!(
            "{engine} amplified output contained {actual} CSV result documents; expected exactly {expected}"
        ));
    }
    Ok(())
}

fn normalize_records(
    header: &[String],
    rows: &[Vec<String>],
    columns: &[(&str, ColumnType)],
    engine: &str,
) -> Result<NormalizedTable, String> {
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
    fn repeated_documents_match_expected_results_and_allow_engine_separators() {
        let columns = [("n", ColumnType::Integer), ("enabled", ColumnType::Boolean)];
        let rusthouse_expected = "n,enabled\n1,true\n";
        let clickhouse_expected = "n,enabled\r\n1,1\r\n";
        let rusthouse_repeated = "n,enabled\n1,true\n\nn,enabled\n1,true\n\nn,enabled\n1,true\n";
        let clickhouse_repeated = "n,enabled\n1,1\nn,enabled\n1,1\nn,enabled\n1,1\n";

        compare_repeated_outputs(
            rusthouse_expected,
            clickhouse_expected,
            rusthouse_repeated,
            clickhouse_repeated,
            &columns,
            3,
        )
        .expect("all repeated documents match");
    }

    #[test]
    fn repeated_documents_require_the_exact_declared_count() {
        let columns = [("n", ColumnType::Integer)];
        let missing_error = compare_repeated_outputs(
            "n\n1\n",
            "n\n1\n",
            "n\n1\nn\n1\n",
            "n\n1\nn\n1\n",
            &columns,
            3,
        )
        .expect_err("missing repetition must fail");
        let extra_error = compare_repeated_outputs(
            "n\n1\n",
            "n\n1\n",
            "n\n1\nn\n1\n",
            "n\n1\nn\n1\n",
            &columns,
            1,
        )
        .expect_err("extra repetition must fail");

        assert!(missing_error.contains("expected exactly 3"));
        assert!(extra_error.contains("contained 2"));
        assert!(extra_error.contains("expected exactly 1"));
    }

    #[test]
    fn every_repetition_must_match_the_unamplified_result() {
        let columns = [("n", ColumnType::Integer)];
        let error = compare_repeated_outputs(
            "n\n1\n",
            "n\n1\n",
            "n\n1\n\nn\n2\n",
            "n\n1\nn\n1\n",
            &columns,
            2,
        )
        .expect_err("changed repeated result must fail");

        assert!(error.contains("RustHouse repetition 2"));
        assert!(error.contains("unamplified expected result"));
    }

    #[test]
    fn every_repetition_must_match_the_peer_engine() {
        let columns = [("value", ColumnType::Float)];
        let error = compare_repeated_outputs(
            "value\n1.0\n",
            "value\n1.0\n",
            "value\n1.0000000009\n",
            "value\n0.9999999991\n",
            &columns,
            1,
        )
        .expect_err("peer difference outside tolerance must fail");

        assert!(error.contains("RustHouse repetition 1"));
        assert!(error.contains("ClickHouse repetition 1"));
    }
}
