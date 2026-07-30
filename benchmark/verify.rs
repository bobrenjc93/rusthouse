use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::config::{BenchmarkSettings, Mode};
use crate::process::{CLICKHOUSE_SHA256, CLICKHOUSE_VERSION};
use crate::score::{RatioObservation, median, parity_score};
use crate::workload::workloads;

const SCHEMA_VERSION: u64 = 3;
const SCORE_TOLERANCE: f64 = 1e-9;
const LIMITATIONS: [&str; 2] = [
    "amplification measures repeated warm in-process work and retains one divided by the amplification factor of startup and setup",
    "synthetic single-process data does not model concurrency, durable storage, networking, joins, nullability, or production compression",
];

pub struct ConsistentDetails {
    pub mode: &'static str,
    pub seed: u64,
    pub case_count: usize,
    pub primary_score: f64,
    pub end_to_end_score: f64,
}

struct ConsistentCase {
    family: String,
    scale: usize,
    primary_ratio: f64,
    end_to_end_ratio: f64,
}

pub fn verify_details_file(path: &Path) -> Result<ConsistentDetails, String> {
    let input = fs::read_to_string(path)
        .map_err(|error| format!("could not read '{}': {error}", path.display()))?;
    verify_details_json(&input)
}

fn verify_details_json(input: &str) -> Result<ConsistentDetails, String> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = StrictValue::deserialize(&mut deserializer)
        .map_err(|error| format!("details are not valid JSON: {error}"))?
        .0;
    deserializer
        .end()
        .map_err(|error| format!("details are not valid JSON: {error}"))?;
    let root = exact_object(
        &value,
        "details",
        &[
            "schema_version",
            "score",
            "primary_score",
            "end_to_end_score",
            "primary_saturated_cases",
            "end_to_end_saturated_cases",
            "mode",
            "seed",
            "warmups",
            "primary_samples",
            "end_to_end_samples",
            "row_counts",
            "timing_method",
            "correctness_checks",
            "rusthouse_path",
            "clickhouse_path",
            "clickhouse_version",
            "clickhouse_sha256",
            "limitations",
            "cases",
        ],
    )?;

    expect_u64(root, "schema_version", SCHEMA_VERSION, "details")?;
    let mode = match string_field(root, "mode", "details")? {
        "quick" => Mode::Quick,
        "default" => Mode::Default,
        value => return Err(format!("details.mode has unknown benchmark mode {value:?}")),
    };
    let settings = mode.settings();
    let seed = u64_field(root, "seed", "details")?;
    verify_configuration(root, &settings)?;

    let expected_cases = expected_case_matrix(&settings);
    expect_usize(root, "correctness_checks", expected_cases.len(), "details")?;
    let cases = array_field(root, "cases", "details")?;
    if cases.len() != expected_cases.len() {
        return Err(format!(
            "details.cases count mismatch: reported {}, expected {} for {} mode",
            cases.len(),
            expected_cases.len(),
            mode.name()
        ));
    }

    let mut seen = BTreeSet::new();
    let mut checked_cases = Vec::with_capacity(cases.len());
    for (index, case) in cases.iter().enumerate() {
        let context = format!("details.cases[{index}]");
        let object = exact_object(
            case,
            &context,
            &[
                "workload",
                "family",
                "row_count",
                "query_amplification",
                "primary",
                "end_to_end",
            ],
        )?;
        let workload = string_field(object, "workload", &context)?.to_owned();
        let family = string_field(object, "family", &context)?.to_owned();
        let row_count = usize_field(object, "row_count", &context)?;
        let identity = (row_count, workload.clone());
        if !seen.insert(identity.clone()) {
            return Err(format!(
                "duplicate benchmark case identity: workload {workload:?} at {row_count} rows"
            ));
        }
        let expected_family = expected_cases.get(&identity).ok_or_else(|| {
            format!("unexpected benchmark case identity: workload {workload:?} at {row_count} rows")
        })?;
        if family != *expected_family {
            return Err(format!(
                "{context}.family mismatch: reported {family:?}, expected {expected_family:?}"
            ));
        }
        expect_usize(
            object,
            "query_amplification",
            settings.query_amplification,
            &context,
        )?;

        let primary = verify_primary(
            field(object, "primary", &context)?,
            settings.samples,
            settings.query_amplification,
            &format!("{context}.primary"),
        )?;
        let end_to_end = verify_end_to_end(
            field(object, "end_to_end", &context)?,
            settings.end_to_end_samples,
            &format!("{context}.end_to_end"),
        )?;
        checked_cases.push(ConsistentCase {
            family,
            scale: row_count,
            primary_ratio: primary,
            end_to_end_ratio: end_to_end,
        });
    }

    if seen.len() != expected_cases.len() {
        let missing = expected_cases
            .keys()
            .find(|identity| !seen.contains(*identity))
            .expect("different equal-sized sets have a missing member");
        return Err(format!(
            "missing benchmark case identity: workload {:?} at {} rows",
            missing.1, missing.0
        ));
    }

    let primary = score(&checked_cases, |case| case.primary_ratio)?;
    let end_to_end = score(&checked_cases, |case| case.end_to_end_ratio)?;
    if mode == Mode::Default && primary.saturated_cases == expected_cases.len() {
        return Err(
            "primary timing saturated: every case reached the parity cap; artifact is not acceptable"
                .to_owned(),
        );
    }
    expect_usize(
        root,
        "primary_saturated_cases",
        primary.saturated_cases,
        "details",
    )?;
    expect_usize(
        root,
        "end_to_end_saturated_cases",
        end_to_end.saturated_cases,
        "details",
    )?;
    expect_score(root, "primary_score", primary.score, "details")?;
    expect_score(root, "score", primary.score, "details")?;
    expect_score(root, "end_to_end_score", end_to_end.score, "details")?;

    Ok(ConsistentDetails {
        mode: mode.name(),
        seed,
        case_count: checked_cases.len(),
        primary_score: primary.score,
        end_to_end_score: end_to_end.score,
    })
}

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object fields")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let number =
            Number::from_f64(value).ok_or_else(|| E::custom("JSON numbers must be finite"))?;
        Ok(StrictValue(Value::Number(number)))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictValue>()? {
            values.push(value.0);
        }
        Ok(StrictValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = entries.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "duplicate JSON object field {key:?}"
                )));
            }
            let value = entries.next_value::<StrictValue>()?;
            object.insert(key, value.0);
        }
        Ok(StrictValue(Value::Object(object)))
    }
}

fn verify_configuration(
    root: &Map<String, Value>,
    settings: &BenchmarkSettings,
) -> Result<(), String> {
    expect_usize(root, "warmups", settings.warmups, "details")?;
    expect_usize(root, "primary_samples", settings.samples, "details")?;
    expect_usize(
        root,
        "end_to_end_samples",
        settings.end_to_end_samples,
        "details",
    )?;
    let row_counts = array_field(root, "row_counts", "details")?;
    let reported_row_counts = row_counts
        .iter()
        .enumerate()
        .map(|(index, value)| value_as_usize(value, &format!("details.row_counts[{index}]")))
        .collect::<Result<Vec<_>, _>>()?;
    if reported_row_counts != settings.row_counts {
        return Err(format!(
            "details.row_counts mismatch: reported {reported_row_counts:?}, expected {:?}",
            settings.row_counts
        ));
    }

    let timing = exact_object(
        field(root, "timing_method", "details")?,
        "details.timing_method",
        &[
            "name",
            "calibration",
            "query_amplification",
            "startup_subtraction",
            "correctness_runs_separate",
            "max_sample_spread",
        ],
    )?;
    expect_string(
        timing,
        "name",
        "in_process_query_amplification",
        "details.timing_method",
    )?;
    expect_string(
        timing,
        "calibration",
        "fixed_shared_repetitions",
        "details.timing_method",
    )?;
    expect_usize(
        timing,
        "query_amplification",
        settings.query_amplification,
        "details.timing_method",
    )?;
    expect_bool(
        timing,
        "startup_subtraction",
        false,
        "details.timing_method",
    )?;
    expect_bool(
        timing,
        "correctness_runs_separate",
        true,
        "details.timing_method",
    )?;
    expect_f64(
        timing,
        "max_sample_spread",
        crate::MAX_SAMPLE_SPREAD,
        "details.timing_method",
    )?;

    let version = string_field(root, "clickhouse_version", "details")?;
    if !version.contains(CLICKHOUSE_VERSION) {
        return Err(format!(
            "details.clickhouse_version mismatch: {version:?} does not identify {CLICKHOUSE_VERSION}"
        ));
    }
    expect_string(root, "clickhouse_sha256", CLICKHOUSE_SHA256, "details")?;
    for path_field in ["rusthouse_path", "clickhouse_path"] {
        if string_field(root, path_field, "details")?.is_empty() {
            return Err(format!("details.{path_field} must not be empty"));
        }
    }
    let limitations = array_field(root, "limitations", "details")?;
    if limitations.len() != LIMITATIONS.len() {
        return Err(format!(
            "details.limitations count mismatch: reported {}, expected {}",
            limitations.len(),
            LIMITATIONS.len()
        ));
    }
    for (index, expected) in LIMITATIONS.iter().enumerate() {
        let reported = limitations[index]
            .as_str()
            .ok_or_else(|| format!("details.limitations[{index}] must be a string"))?;
        if reported != *expected {
            return Err(format!(
                "details.limitations[{index}] mismatch: reported {reported:?}, expected {expected:?}"
            ));
        }
    }
    Ok(())
}

fn verify_primary(
    value: &Value,
    sample_count: usize,
    amplification: usize,
    context: &str,
) -> Result<f64, String> {
    let object = exact_object(
        value,
        context,
        &[
            "rusthouse_batch_median_ms",
            "clickhouse_batch_median_ms",
            "rusthouse_per_query_median_ms",
            "clickhouse_per_query_median_ms",
            "clickhouse_rusthouse_ratio",
            "rusthouse_batch_samples_ms",
            "clickhouse_batch_samples_ms",
            "rusthouse_per_query_samples_ms",
            "clickhouse_per_query_samples_ms",
        ],
    )?;
    let rusthouse_batch =
        number_array(object, "rusthouse_batch_samples_ms", sample_count, context)?;
    let clickhouse_batch =
        number_array(object, "clickhouse_batch_samples_ms", sample_count, context)?;
    let rusthouse_query = number_array(
        object,
        "rusthouse_per_query_samples_ms",
        sample_count,
        context,
    )?;
    let clickhouse_query = number_array(
        object,
        "clickhouse_per_query_samples_ms",
        sample_count,
        context,
    )?;
    verify_amortized_samples(
        &rusthouse_batch,
        &rusthouse_query,
        amplification,
        &format!("{context}.rusthouse_per_query_samples_ms"),
    )?;
    verify_amortized_samples(
        &clickhouse_batch,
        &clickhouse_query,
        amplification,
        &format!("{context}.clickhouse_per_query_samples_ms"),
    )?;

    let rusthouse_batch_median = stable_median(&rusthouse_batch, context)?;
    let clickhouse_batch_median = stable_median(&clickhouse_batch, context)?;
    let rusthouse_query_median = stable_median(&rusthouse_query, context)?;
    let clickhouse_query_median = stable_median(&clickhouse_query, context)?;
    expect_f64(
        object,
        "rusthouse_batch_median_ms",
        rusthouse_batch_median,
        context,
    )?;
    expect_f64(
        object,
        "clickhouse_batch_median_ms",
        clickhouse_batch_median,
        context,
    )?;
    expect_f64(
        object,
        "rusthouse_per_query_median_ms",
        rusthouse_query_median,
        context,
    )?;
    expect_f64(
        object,
        "clickhouse_per_query_median_ms",
        clickhouse_query_median,
        context,
    )?;
    let ratio = clickhouse_query_median / rusthouse_query_median;
    expect_f64(object, "clickhouse_rusthouse_ratio", ratio, context)?;
    Ok(ratio)
}

fn verify_end_to_end(value: &Value, sample_count: usize, context: &str) -> Result<f64, String> {
    let object = exact_object(
        value,
        context,
        &[
            "rusthouse_median_ms",
            "clickhouse_median_ms",
            "clickhouse_rusthouse_ratio",
            "rusthouse_samples_ms",
            "clickhouse_samples_ms",
        ],
    )?;
    let rusthouse = number_array(object, "rusthouse_samples_ms", sample_count, context)?;
    let clickhouse = number_array(object, "clickhouse_samples_ms", sample_count, context)?;
    let rusthouse_median = stable_median(&rusthouse, context)?;
    let clickhouse_median = stable_median(&clickhouse, context)?;
    expect_f64(object, "rusthouse_median_ms", rusthouse_median, context)?;
    expect_f64(object, "clickhouse_median_ms", clickhouse_median, context)?;
    let ratio = clickhouse_median / rusthouse_median;
    expect_f64(object, "clickhouse_rusthouse_ratio", ratio, context)?;
    Ok(ratio)
}

fn verify_amortized_samples(
    batches: &[f64],
    per_query: &[f64],
    amplification: usize,
    context: &str,
) -> Result<(), String> {
    for (index, (batch, reported)) in batches.iter().zip(per_query).enumerate() {
        let recomputed = batch / amplification as f64;
        if reported.to_bits() != recomputed.to_bits() {
            return Err(format!(
                "{context}[{index}] mismatch: reported {reported}, recomputed {recomputed} from the raw batch sample"
            ));
        }
    }
    Ok(())
}

fn stable_median(samples: &[f64], context: &str) -> Result<f64, String> {
    let value = median(samples).map_err(|error| format!("{context}: {error}"))?;
    if value <= 0.0 {
        return Err(format!("{context}: median timing must be positive"));
    }
    let minimum = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if maximum / minimum > crate::MAX_SAMPLE_SPREAD {
        return Err(format!(
            "{context}: unstable timing max/min spread {:.2} exceeds {:.2}",
            maximum / minimum,
            crate::MAX_SAMPLE_SPREAD
        ));
    }
    Ok(value)
}

fn score(
    cases: &[ConsistentCase],
    ratio: impl Fn(&ConsistentCase) -> f64,
) -> Result<crate::score::ScoreBreakdown, String> {
    let observations = cases
        .iter()
        .map(|case| RatioObservation {
            family: case.family.as_str(),
            scale: case.scale,
            ratio: ratio(case),
        })
        .collect::<Vec<_>>();
    parity_score(&observations)
}

fn expected_case_matrix(settings: &BenchmarkSettings) -> BTreeMap<(usize, String), String> {
    let mut expected = BTreeMap::new();
    for row_count in settings.row_counts.iter().copied() {
        for workload in workloads(row_count) {
            expected.insert(
                (row_count, workload.name.to_owned()),
                workload.family.name().to_owned(),
            );
        }
    }
    expected
}

fn exact_object<'a>(
    value: &'a Value,
    context: &str,
    expected_keys: &[&str],
) -> Result<&'a Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{context} must be a JSON object"))?;
    for key in expected_keys {
        if !object.contains_key(*key) {
            return Err(format!("{context} is missing field {key:?}"));
        }
    }
    if object.len() != expected_keys.len() {
        let unexpected = object
            .keys()
            .find(|key| !expected_keys.contains(&key.as_str()))
            .expect("different key counts imply an unexpected key");
        return Err(format!(
            "{context} contains unexpected field {unexpected:?}"
        ));
    }
    Ok(object)
}

fn field<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a Value, String> {
    object
        .get(key)
        .ok_or_else(|| format!("{context} is missing field {key:?}"))
}

fn array_field<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a Vec<Value>, String> {
    field(object, key, context)?
        .as_array()
        .ok_or_else(|| format!("{context}.{key} must be an array"))
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a str, String> {
    field(object, key, context)?
        .as_str()
        .ok_or_else(|| format!("{context}.{key} must be a string"))
}

fn usize_field(object: &Map<String, Value>, key: &str, context: &str) -> Result<usize, String> {
    value_as_usize(field(object, key, context)?, &format!("{context}.{key}"))
}

fn value_as_usize(value: &Value, context: &str) -> Result<usize, String> {
    let value = value
        .as_u64()
        .ok_or_else(|| format!("{context} must be an unsigned integer"))?;
    usize::try_from(value).map_err(|_| format!("{context} is too large"))
}

fn u64_field(object: &Map<String, Value>, key: &str, context: &str) -> Result<u64, String> {
    field(object, key, context)?
        .as_u64()
        .ok_or_else(|| format!("{context}.{key} must be an unsigned integer"))
}

fn f64_field(object: &Map<String, Value>, key: &str, context: &str) -> Result<f64, String> {
    let value = field(object, key, context)?
        .as_f64()
        .ok_or_else(|| format!("{context}.{key} must be a number"))?;
    if !value.is_finite() {
        return Err(format!("{context}.{key} must be finite"));
    }
    Ok(value)
}

fn number_array(
    object: &Map<String, Value>,
    key: &str,
    expected_len: usize,
    context: &str,
) -> Result<Vec<f64>, String> {
    let values = array_field(object, key, context)?;
    if values.len() != expected_len {
        return Err(format!(
            "{context}.{key} sample count mismatch: reported {}, expected {expected_len}",
            values.len()
        ));
    }
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let number = value
                .as_f64()
                .filter(|number| number.is_finite() && *number > 0.0)
                .ok_or_else(|| format!("{context}.{key}[{index}] must be finite and positive"))?;
            Ok(number)
        })
        .collect()
}

fn expect_u64(
    object: &Map<String, Value>,
    key: &str,
    expected: u64,
    context: &str,
) -> Result<(), String> {
    let reported = u64_field(object, key, context)?;
    if reported != expected {
        return Err(format!(
            "{context}.{key} mismatch: reported {reported}, expected {expected}"
        ));
    }
    Ok(())
}

fn expect_usize(
    object: &Map<String, Value>,
    key: &str,
    expected: usize,
    context: &str,
) -> Result<(), String> {
    let reported = usize_field(object, key, context)?;
    if reported != expected {
        return Err(format!(
            "{context}.{key} mismatch: reported {reported}, expected {expected}"
        ));
    }
    Ok(())
}

fn expect_f64(
    object: &Map<String, Value>,
    key: &str,
    expected: f64,
    context: &str,
) -> Result<(), String> {
    let reported = f64_field(object, key, context)?;
    if reported.to_bits() != expected.to_bits() {
        return Err(format!(
            "{context}.{key} mismatch: reported {reported}, recomputed {expected}"
        ));
    }
    Ok(())
}

fn expect_score(
    object: &Map<String, Value>,
    key: &str,
    expected: f64,
    context: &str,
) -> Result<(), String> {
    let reported = f64_field(object, key, context)?;
    if !scores_match(reported, expected) {
        return Err(format!(
            "{context}.{key} mismatch: reported {reported}, recomputed {expected}, tolerance {SCORE_TOLERANCE}"
        ));
    }
    Ok(())
}

fn scores_match(reported: f64, expected: f64) -> bool {
    (reported - expected).abs() <= SCORE_TOLERANCE
}

fn expect_string(
    object: &Map<String, Value>,
    key: &str,
    expected: &str,
    context: &str,
) -> Result<(), String> {
    let reported = string_field(object, key, context)?;
    if reported != expected {
        return Err(format!(
            "{context}.{key} mismatch: reported {reported:?}, expected {expected:?}"
        ));
    }
    Ok(())
}

fn expect_bool(
    object: &Map<String, Value>,
    key: &str,
    expected: bool,
    context: &str,
) -> Result<(), String> {
    let reported = field(object, key, context)?
        .as_bool()
        .ok_or_else(|| format!("{context}.{key} must be Boolean"))?;
    if reported != expected {
        return Err(format!(
            "{context}.{key} mismatch: reported {reported}, expected {expected}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchical_score_comparison_has_a_cross_platform_tolerance() {
        let expected = 99.705_101_944_396_18;
        assert!(scores_match(expected + SCORE_TOLERANCE / 2.0, expected));
        assert!(!scores_match(expected + SCORE_TOLERANCE * 2.0, expected));
    }
}
