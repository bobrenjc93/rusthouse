#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Float,
    Boolean,
    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeCompatibility {
    pub rusthouse: &'static [&'static str],
    pub clickhouse: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: &'static str,
    pub value_type: ColumnType,
    pub compatible_types: TypeCompatibility,
}

impl ColumnSpec {
    pub const fn new(
        name: &'static str,
        value_type: ColumnType,
        rusthouse_types: &'static [&'static str],
        clickhouse_types: &'static [&'static str],
    ) -> Self {
        Self {
            name,
            value_type,
            compatible_types: TypeCompatibility {
                rusthouse: rusthouse_types,
                clickhouse: clickhouse_types,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedColumn {
    pub name: String,
    pub data_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSchemas {
    pub rusthouse: Vec<ObservedColumn>,
    pub clickhouse: Vec<ObservedColumn>,
}

#[derive(Debug, Clone, PartialEq)]
enum NormalizedValue {
    Integer(i128),
    Float(f64),
    Boolean(bool),
    String(String),
}

#[derive(Debug, Clone, PartialEq)]
struct ParsedTable {
    schema: Vec<ObservedColumn>,
    rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy)]
enum Engine {
    RustHouse,
    ClickHouse,
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Self::RustHouse => "RustHouse",
            Self::ClickHouse => "ClickHouse",
        }
    }

    fn compatible_types(self, column: &ColumnSpec) -> &'static [&'static str] {
        match self {
            Self::RustHouse => column.compatible_types.rusthouse,
            Self::ClickHouse => column.compatible_types.clickhouse,
        }
    }
}

pub fn compare_outputs(
    rusthouse_csv: &str,
    clickhouse_csv: &str,
    columns: &[ColumnSpec],
) -> Result<OutputSchemas, String> {
    let rusthouse = parse_typed_csv(rusthouse_csv, Engine::RustHouse)?;
    let clickhouse = parse_typed_csv(clickhouse_csv, Engine::ClickHouse)?;

    // Schemas are deliberately checked before any values are interpreted.
    validate_schema(&rusthouse.schema, columns, Engine::RustHouse)?;
    validate_schema(&clickhouse.schema, columns, Engine::ClickHouse)?;

    let rusthouse_rows = normalize_rows(&rusthouse.rows, columns, Engine::RustHouse)?;
    let clickhouse_rows = normalize_rows(&clickhouse.rows, columns, Engine::ClickHouse)?;

    if rusthouse_rows.len() != clickhouse_rows.len() {
        return Err(format!(
            "row count mismatch: RustHouse returned {}, ClickHouse returned {}",
            rusthouse_rows.len(),
            clickhouse_rows.len()
        ));
    }

    for (row_index, (left, right)) in rusthouse_rows.iter().zip(&clickhouse_rows).enumerate() {
        for (column_index, (left, right)) in left.iter().zip(right).enumerate() {
            if !values_equal(left, right) {
                return Err(format!(
                    "result mismatch at row {}, column '{}': RustHouse={left:?}, ClickHouse={right:?}",
                    row_index + 1,
                    columns[column_index].name
                ));
            }
        }
    }

    Ok(OutputSchemas {
        rusthouse: rusthouse.schema,
        clickhouse: clickhouse.schema,
    })
}

fn parse_typed_csv(csv: &str, engine: Engine) -> Result<ParsedTable, String> {
    let records = parse_csv(csv).map_err(|error| format!("{} CSV: {error}", engine.name()))?;
    let (names, remaining) = records
        .split_first()
        .ok_or_else(|| format!("{} returned no CSV name row", engine.name()))?;
    let (types, rows) = remaining
        .split_first()
        .ok_or_else(|| format!("{} returned no CSV type row", engine.name()))?;
    if names.len() != types.len() {
        return Err(format!(
            "{} schema width mismatch: name row has {} columns, type row has {}",
            engine.name(),
            names.len(),
            types.len()
        ));
    }

    Ok(ParsedTable {
        schema: names
            .iter()
            .cloned()
            .zip(types.iter().cloned())
            .map(|(name, data_type)| ObservedColumn { name, data_type })
            .collect(),
        rows: rows.to_vec(),
    })
}

fn validate_schema(
    observed: &[ObservedColumn],
    columns: &[ColumnSpec],
    engine: Engine,
) -> Result<(), String> {
    let observed_names = observed
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    let expected_names = columns.iter().map(|column| column.name).collect::<Vec<_>>();
    if observed_names != expected_names {
        return Err(format!(
            "{} header mismatch: expected {expected_names:?}, got {observed_names:?}",
            engine.name()
        ));
    }

    for (observed, expected) in observed.iter().zip(columns) {
        let compatible_types = engine.compatible_types(expected);
        if !compatible_types.contains(&observed.data_type.as_str()) {
            return Err(format!(
                "{} type mismatch for column '{}': observed {:?}, compatible types are {compatible_types:?}",
                engine.name(),
                expected.name,
                observed.data_type
            ));
        }
    }
    Ok(())
}

fn normalize_rows(
    rows: &[Vec<String>],
    columns: &[ColumnSpec],
    engine: Engine,
) -> Result<Vec<Vec<NormalizedValue>>, String> {
    let mut normalized_rows = Vec::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        if row.len() != columns.len() {
            return Err(format!(
                "{} row {} has {} columns; expected {}",
                engine.name(),
                row_index + 1,
                row.len(),
                columns.len()
            ));
        }
        let normalized = row
            .iter()
            .zip(columns)
            .enumerate()
            .map(|(column_index, (value, column))| {
                normalize_value(value, column.value_type).map_err(|error| {
                    format!(
                        "{} row {}, column '{}': {error}",
                        engine.name(),
                        row_index + 1,
                        columns[column_index].name
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        normalized_rows.push(normalized);
    }
    Ok(normalized_rows)
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

    const INT64: &[&str] = &["Int64"];
    const UINT64: &[&str] = &["UInt64"];
    const FLOAT64: &[&str] = &["Float64"];
    const BOOL: &[&str] = &["Bool"];
    const STRING: &[&str] = &["String"];

    #[test]
    fn normalizes_engine_specific_scalar_spellings_and_csv_escaping() {
        let columns = [
            ColumnSpec::new("n", ColumnType::Integer, INT64, UINT64),
            ColumnSpec::new("mean", ColumnType::Float, FLOAT64, FLOAT64),
            ColumnSpec::new("enabled", ColumnType::Boolean, BOOL, BOOL),
            ColumnSpec::new("label", ColumnType::String, STRING, STRING),
        ];
        let rusthouse = "n,mean,enabled,label\nInt64,Float64,Bool,String\n2,1.0,true,\"comma, and \"\"quote\"\"\"\n";
        let clickhouse = "n,mean,enabled,label\r\nUInt64,Float64,Bool,String\r\n2,1,1,\"comma, and \"\"quote\"\"\"\r\n";

        let schemas = compare_outputs(rusthouse, clickhouse, &columns).expect("equivalent output");
        assert_eq!(schemas.rusthouse[0].data_type, "Int64");
        assert_eq!(schemas.clickhouse[0].data_type, "UInt64");
    }

    #[test]
    fn float_comparison_allows_rendering_noise_but_not_real_differences() {
        let columns = [ColumnSpec::new("mean", ColumnType::Float, FLOAT64, FLOAT64)];
        compare_outputs(
            "mean\nFloat64\n0.3333333333333333\n",
            "mean\nFloat64\n0.33333333333333331\n",
            &columns,
        )
        .expect("rendering-only difference");
        assert!(
            compare_outputs("mean\nFloat64\n1.0\n", "mean\nFloat64\n1.01\n", &columns).is_err()
        );
    }

    #[test]
    fn rejects_textual_matches_with_incompatible_schemas() {
        let columns = [ColumnSpec::new("value", ColumnType::Integer, INT64, INT64)];
        let error = compare_outputs("value\nInt64\n1\n", "value\nString\n1\n", &columns)
            .expect_err("a textual match must not hide a type mismatch");

        assert!(error.contains("ClickHouse type mismatch"));
    }

    #[test]
    fn validates_both_schemas_before_parsing_values() {
        let columns = [ColumnSpec::new("value", ColumnType::Integer, INT64, INT64)];
        let error = compare_outputs(
            "value\nInt64\nnot-an-integer\n",
            "value\nString\nnot-an-integer\n",
            &columns,
        )
        .expect_err("schema validation must run before cell parsing");

        assert!(error.contains("ClickHouse type mismatch"));
        assert!(!error.contains("invalid integer"));
    }

    #[test]
    fn rejects_malformed_or_mismatched_output() {
        let columns = [ColumnSpec::new("value", ColumnType::String, STRING, STRING)];
        assert!(
            compare_outputs("value\nString\nleft\n", "value\nString\nright\n", &columns).is_err()
        );
        assert!(
            compare_outputs("wrong\nString\nleft\n", "value\nString\nleft\n", &columns).is_err()
        );
        assert!(
            compare_outputs(
                "value\nString\n\"unfinished\n",
                "value\nString\nleft\n",
                &columns
            )
            .is_err()
        );
        assert!(compare_outputs("value\n", "value\nString\nleft\n", &columns).is_err());
    }
}
