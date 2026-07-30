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

pub struct CorpusResult<'a> {
    pub name: &'a str,
    pub columns: &'a [(String, ColumnType)],
    pub max_rows: usize,
}

pub fn compare_outputs(
    rusthouse_csv: &str,
    clickhouse_csv: &str,
    columns: &[(&str, ColumnType)],
) -> Result<(), String> {
    let rusthouse = normalize(rusthouse_csv, columns, "RustHouse")?;
    let clickhouse = normalize(clickhouse_csv, columns, "ClickHouse")?;

    compare_tables(&rusthouse, &clickhouse, columns, None)
}

pub fn compare_corpus_outputs(
    rusthouse_csv: &str,
    clickhouse_csv: &str,
    results: &[CorpusResult<'_>],
) -> Result<(), String> {
    if results.is_empty() {
        return Err("correctness corpus has no expected results".to_owned());
    }
    let rusthouse = normalize_corpus(rusthouse_csv, results, "RustHouse")?;
    let clickhouse = normalize_corpus(clickhouse_csv, results, "ClickHouse")?;

    for (index, result) in results.iter().enumerate() {
        let columns = result
            .columns
            .iter()
            .map(|(name, column_type)| (name.as_str(), *column_type))
            .collect::<Vec<_>>();
        compare_tables(
            &rusthouse[index],
            &clickhouse[index],
            &columns,
            Some(result.name),
        )?;
    }
    Ok(())
}

fn compare_tables(
    rusthouse: &NormalizedTable,
    clickhouse: &NormalizedTable,
    columns: &[(&str, ColumnType)],
    result_name: Option<&str>,
) -> Result<(), String> {
    let context = result_name
        .map(|name| format!(" in audit query '{name}'"))
        .unwrap_or_default();

    if rusthouse.rows.len() != clickhouse.rows.len() {
        return Err(format!(
            "row count mismatch{context}: RustHouse returned {}, ClickHouse returned {}",
            rusthouse.rows.len(),
            clickhouse.rows.len()
        ));
    }

    for (row_index, (left, right)) in rusthouse.rows.iter().zip(&clickhouse.rows).enumerate() {
        for (column_index, (left, right)) in left.iter().zip(right).enumerate() {
            if !values_equal(left, right) {
                return Err(format!(
                    "result mismatch{context} at row {}, column '{}': RustHouse={left:?}, ClickHouse={right:?}",
                    row_index + 1,
                    columns[column_index].0
                ));
            }
        }
    }
    Ok(())
}

fn normalize_corpus(
    csv: &str,
    results: &[CorpusResult<'_>],
    engine: &str,
) -> Result<Vec<NormalizedTable>, String> {
    let records = parse_csv(csv).map_err(|error| format!("{engine} CSV: {error}"))?;
    let mut cursor = 0;
    let mut tables = Vec::with_capacity(results.len());

    for (index, result) in results.iter().enumerate() {
        let columns = result
            .columns
            .iter()
            .map(|(name, column_type)| (name.as_str(), *column_type))
            .collect::<Vec<_>>();
        let header = records.get(cursor).ok_or_else(|| {
            format!(
                "{engine} returned only {index} of {} audit results; missing '{}'",
                results.len(),
                result.name
            )
        })?;
        if !header_matches(header, &columns) {
            let expected = columns.iter().map(|(name, _)| *name).collect::<Vec<_>>();
            return Err(format!(
                "{engine} audit header mismatch for '{}': expected {expected:?}, got {header:?}",
                result.name
            ));
        }

        let (end, next_cursor) = if let Some(next) = results.get(index + 1) {
            let next_columns = next
                .columns
                .iter()
                .map(|(name, column_type)| (name.as_str(), *column_type))
                .collect::<Vec<_>>();
            let next_header = records[cursor + 1..]
                .iter()
                .position(|record| header_matches(record, &next_columns))
                .map(|offset| cursor + 1 + offset)
                .ok_or_else(|| {
                    format!(
                        "{engine} output has no header for audit query '{}' after '{}'",
                        next.name, result.name
                    )
                })?;
            let result_end =
                if next_header > cursor + 1 && is_multiquery_separator(&records[next_header - 1]) {
                    next_header - 1
                } else {
                    next_header
                };
            (result_end, next_header)
        } else {
            (records.len(), records.len())
        };
        let row_count = end - cursor - 1;
        if row_count > result.max_rows {
            return Err(format!(
                "{engine} audit query '{}' returned {row_count} rows; bounded maximum is {}",
                result.name, result.max_rows
            ));
        }
        tables.push(normalize_records(
            &records[cursor..end],
            &columns,
            &format!("{engine} audit query '{}'", result.name),
        )?);
        cursor = next_cursor;
    }

    if cursor != records.len() {
        return Err(format!(
            "{engine} emitted {} unexpected CSV records after the audit corpus",
            records.len() - cursor
        ));
    }
    Ok(tables)
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
    if !header_matches(header, columns) {
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

fn header_matches(header: &[String], columns: &[(&str, ColumnType)]) -> bool {
    header
        .iter()
        .map(String::as_str)
        .eq(columns.iter().map(|(name, _)| *name))
}

fn is_multiquery_separator(record: &[String]) -> bool {
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
    fn compares_each_bounded_result_in_a_multiquery_stream() {
        let first_columns = vec![("q0000_n".to_owned(), ColumnType::Integer)];
        let second_columns = vec![("q0001_flag".to_owned(), ColumnType::Boolean)];
        let results = [
            CorpusResult {
                name: "q0000",
                columns: &first_columns,
                max_rows: 2,
            },
            CorpusResult {
                name: "q0001",
                columns: &second_columns,
                max_rows: 1,
            },
        ];
        let rusthouse = "q0000_n\n1\n2\n\nq0001_flag\ntrue\n";
        let clickhouse = "q0000_n\r\n1\r\n2\r\nq0001_flag\r\n1\r\n";

        compare_corpus_outputs(rusthouse, clickhouse, &results).expect("matching corpus");
    }

    #[test]
    fn corpus_comparison_fails_on_missing_unbounded_or_mismatched_results() {
        let columns = vec![("q0000_n".to_owned(), ColumnType::Integer)];
        let results = [CorpusResult {
            name: "q0000",
            columns: &columns,
            max_rows: 1,
        }];

        assert!(compare_corpus_outputs("", "", &results).is_err());
        assert!(compare_corpus_outputs("q0000_n\n1\n2\n", "q0000_n\n1\n", &results).is_err());
        assert!(compare_corpus_outputs("q0000_n\n1\n", "q0000_n\n2\n", &results).is_err());
    }
}
