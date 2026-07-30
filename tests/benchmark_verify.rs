#![cfg(feature = "benchmark-verifier")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

fn evidence_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmark/results/default-20260729.json")
}

fn verifier(path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clickhouse-parity-bench"))
        .args(["--verify-details", path.to_str().expect("UTF-8 test path")])
        .env("RUSTHOUSE_CLICKHOUSE_BIN", "/definitely/missing/clickhouse")
        .env("RUSTHOUSE_BIN", "/definitely/missing/rusthouse")
        .output()
        .expect("run details verifier")
}

fn read_details() -> Value {
    serde_json::from_str(&fs::read_to_string(evidence_path()).expect("read retained evidence"))
        .expect("parse retained evidence")
}

fn temp_details(details: &Value) -> PathBuf {
    let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "rusthouse-benchmark-verifier-{}-{id}.json",
        std::process::id()
    ));
    fs::write(
        &path,
        serde_json::to_vec(details).expect("serialize test details"),
    )
    .expect("write test details");
    path
}

fn assert_rejected(details: &Value, expected_error: &str) {
    let path = temp_details(details);
    let output = verifier(&path);
    fs::remove_file(path).expect("remove test details");
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).expect("failure JSON report");
    let evidence = report["evidence"]
        .as_array()
        .expect("failure evidence array")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        evidence.contains(expected_error),
        "expected {expected_error:?} in {evidence:?}"
    );
}

#[test]
fn checked_in_evidence_is_consistent_without_engine_executables() {
    let output = verifier(&evidence_path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("success JSON report");
    let reported_score = report["score"].as_f64().expect("report score");
    let retained_score = read_details()["score"].as_f64().expect("retained score");
    assert!((reported_score - retained_score).abs() < 0.000_001);
    assert!(
        report["summary"]
            .as_str()
            .expect("summary")
            .contains("Arithmetic-consistent default benchmark details")
    );
    assert!(
        report["evidence"]
            .as_array()
            .expect("evidence")
            .iter()
            .filter_map(Value::as_str)
            .any(|message| message.contains("does not authenticate"))
    );
}

#[test]
fn tampering_fixtures_are_rejected() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for fixture in [
        "changed-sample.json",
        "changed-score.json",
        "changed-configuration.json",
        "changed-case-identity.json",
    ] {
        let patch: Value = serde_json::from_str(
            &fs::read_to_string(manifest.join("benchmark/fixtures").join(fixture))
                .expect("read tampering fixture"),
        )
        .expect("parse tampering fixture");
        let mut details = read_details();
        let pointer = patch["pointer"].as_str().expect("fixture pointer");
        *details
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("fixture pointer {pointer:?}")) =
            patch["replacement"].clone();
        assert_rejected(
            &details,
            patch["expected_error"].as_str().expect("expected error"),
        );
    }
}

#[test]
fn duplicate_and_missing_cases_are_rejected() {
    let mut duplicate = read_details();
    let cases = duplicate["cases"].as_array_mut().expect("cases");
    cases[1] = cases[0].clone();
    assert_rejected(&duplicate, "duplicate benchmark case identity");

    let mut missing = read_details();
    missing["cases"].as_array_mut().expect("cases").pop();
    assert_rejected(&missing, "details.cases count mismatch");
}

#[test]
fn duplicate_json_fields_are_rejected() {
    let details = fs::read_to_string(evidence_path()).expect("read retained evidence");
    let tampered = details.replacen("\"score\":", "\"score\":1,\"score\":", 1);
    let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "rusthouse-benchmark-verifier-duplicate-field-{}-{id}.json",
        std::process::id()
    ));
    fs::write(&path, tampered).expect("write duplicate-field details");
    let output = verifier(&path);
    fs::remove_file(path).expect("remove duplicate-field details");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("duplicate JSON object field"));
}

#[test]
fn self_consistent_fabrication_is_not_claimed_as_authenticated() {
    let mut details = read_details();
    details["seed"] = Value::from(123_u64);
    details["rusthouse_path"] = Value::from("/fabricated/rusthouse");
    details["clickhouse_path"] = Value::from("/fabricated/clickhouse");
    for case in details["cases"].as_array_mut().expect("cases") {
        let primary = &mut case["primary"];
        for field in [
            "rusthouse_batch_median_ms",
            "clickhouse_batch_median_ms",
            "rusthouse_per_query_median_ms",
            "clickhouse_per_query_median_ms",
        ] {
            scale_number(&mut primary[field], 2.0);
        }
        for field in [
            "rusthouse_batch_samples_ms",
            "clickhouse_batch_samples_ms",
            "rusthouse_per_query_samples_ms",
            "clickhouse_per_query_samples_ms",
        ] {
            for sample in primary[field].as_array_mut().expect("primary samples") {
                scale_number(sample, 2.0);
            }
        }

        let end_to_end = &mut case["end_to_end"];
        for field in ["rusthouse_median_ms", "clickhouse_median_ms"] {
            scale_number(&mut end_to_end[field], 2.0);
        }
        for field in ["rusthouse_samples_ms", "clickhouse_samples_ms"] {
            for sample in end_to_end[field]
                .as_array_mut()
                .expect("end-to-end samples")
            {
                scale_number(sample, 2.0);
            }
        }
    }
    let path = temp_details(&details);
    let output = verifier(&path);
    fs::remove_file(path).expect("remove metadata test details");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 report");
    assert!(stdout.contains("Arithmetic-consistent"));
    assert!(stdout.contains("does not authenticate the claimed seed, paths, binaries"));
}

fn scale_number(value: &mut Value, factor: f64) {
    *value = Value::from(value.as_f64().expect("number") * factor);
}

#[test]
fn hierarchical_scores_allow_only_the_defined_rounding_tolerance() {
    let mut within_tolerance = read_details();
    for field in ["score", "primary_score", "end_to_end_score"] {
        let value = within_tolerance[field].as_f64().expect("score");
        within_tolerance[field] = Value::from(value + 0.000_000_000_5);
    }
    let path = temp_details(&within_tolerance);
    let output = verifier(&path);
    fs::remove_file(path).expect("remove score tolerance details");
    assert!(output.status.success());

    let mut outside_tolerance = read_details();
    outside_tolerance["score"] =
        Value::from(outside_tolerance["score"].as_f64().expect("score") + 0.000_000_002);
    assert_rejected(&outside_tolerance, "details.score mismatch");
}
