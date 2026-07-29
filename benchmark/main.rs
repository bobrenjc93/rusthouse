mod config;
mod dataset;
mod normalize;
mod process;
mod score;
mod workload;

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(test)]
use std::time::Duration;

use config::{Config, ParseResult};
use dataset::Dataset;
use normalize::{ColumnType, compare_outputs};
use process::{ClickHouseIdentity, Engine, EnginePaths, TimedOutput};
use score::{median, parity_score};
use workload::workloads;

const HELP: &str = "\
RustHouse / ClickHouse Local black-box parity benchmark

USAGE:
    clickhouse-parity-bench [OPTIONS]

OPTIONS:
    --mode <quick|default>  Benchmark size (default: default)
    --quick                 Alias for --mode quick
    --seed <U64>            Deterministic runtime seed (default: 20260729)
    --clickhouse <PATH>     ClickHouse 26.7.1 binary
    --rusthouse <PATH>      Prebuilt rusthouse CLI (default: sibling binary)
    --details <PATH>        Write detailed JSON without changing stdout
    -h, --help              Print this help

RUSTHOUSE_CLICKHOUSE_BIN supplies --clickhouse when the flag is absent.
RUSTHOUSE_BIN supplies --rusthouse when the flag is absent.
Build release binaries before benchmarking; compilation is never timed.
";

#[derive(Debug)]
struct CaseResult {
    workload: &'static str,
    family: &'static str,
    row_count: usize,
    rusthouse_samples_ms: Vec<f64>,
    clickhouse_samples_ms: Vec<f64>,
    rusthouse_median_ms: f64,
    clickhouse_median_ms: f64,
    ratio: f64,
}

struct Report {
    score: f64,
    summary: String,
    evidence: Vec<String>,
    suggestions: Vec<String>,
}

fn main() -> ExitCode {
    let default_rusthouse = match default_rusthouse_path() {
        Ok(path) => path,
        Err(error) => return emit_failure(error),
    };
    let parsed = config::parse(
        env::args().skip(1),
        env::var("RUSTHOUSE_CLICKHOUSE_BIN").ok(),
        env::var("RUSTHOUSE_BIN").ok(),
        default_rusthouse,
    );
    let config = match parsed {
        Ok(ParseResult::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(ParseResult::Run(config)) => config,
        Err(error) => return emit_failure(error),
    };

    match run(config) {
        Ok(report) => {
            println!("{}", report.to_json());
            ExitCode::SUCCESS
        }
        Err(error) => emit_failure(error),
    }
}

fn emit_failure(error: String) -> ExitCode {
    let report = Report {
        score: 0.0,
        summary: "Benchmark rejected: no timing score was accepted.".to_owned(),
        evidence: vec![error],
        suggestions: vec![
            "Fix the reported setup or correctness failure and rerun the identical command."
                .to_owned(),
        ],
    };
    println!("{}", report.to_json());
    ExitCode::FAILURE
}

fn default_rusthouse_path() -> Result<PathBuf, String> {
    let executable = env::current_exe()
        .map_err(|error| format!("cannot locate benchmark executable: {error}"))?;
    let directory = executable
        .parent()
        .ok_or_else(|| "benchmark executable has no parent directory".to_owned())?;
    Ok(directory.join(format!("rusthouse{}", env::consts::EXE_SUFFIX)))
}

fn run(config: Config) -> Result<Report, String> {
    let settings = config.mode.settings();
    let paths = EnginePaths {
        rusthouse: config.rusthouse.clone(),
        clickhouse: config.clickhouse.clone(),
    };
    let identity = paths.validate()?;
    let mut cases = Vec::new();
    let mut correctness_checks = 0_usize;

    for (row_count_index, row_count) in settings.row_counts.iter().copied().enumerate() {
        let dataset_seed = config.seed ^ (row_count as u64).wrapping_mul(0xd6e8_feb8_6659_fd93);
        let dataset = Dataset::generate(dataset_seed, row_count);
        let setup_sql = dataset.setup_sql();

        for (workload_index, workload) in workloads(row_count).into_iter().enumerate() {
            eprintln!(
                "benchmarking {} at {} rows ({} warmups, {} samples)",
                workload.name, row_count, settings.warmups, settings.samples
            );
            let mut rusthouse_samples = Vec::with_capacity(settings.samples);
            let mut clickhouse_samples = Vec::with_capacity(settings.samples);
            let iterations = settings.warmups + settings.samples;

            for iteration in 0..iterations {
                let rusthouse_first = (row_count_index + workload_index + iteration) % 2 == 0;
                let (rusthouse, clickhouse) =
                    execute_pair(&paths, &setup_sql, &workload.sql, rusthouse_first)?;
                let record = iteration >= settings.warmups;
                accept_sample(
                    &workload.columns,
                    &rusthouse,
                    &clickhouse,
                    record,
                    &mut rusthouse_samples,
                    &mut clickhouse_samples,
                )
                .map_err(|error| {
                    format!(
                        "correctness gate failed for '{}' at {row_count} rows: {error}",
                        workload.name
                    )
                })?;
                correctness_checks += 1;
            }

            let rusthouse_median = median(&rusthouse_samples)?;
            let clickhouse_median = median(&clickhouse_samples)?;
            if rusthouse_median <= 0.0 || clickhouse_median <= 0.0 {
                return Err(format!(
                    "timer resolution was insufficient for '{}' at {row_count} rows",
                    workload.name
                ));
            }
            let ratio = clickhouse_median / rusthouse_median;
            eprintln!(
                "  medians: RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}",
                rusthouse_median, clickhouse_median, ratio
            );
            cases.push(CaseResult {
                workload: workload.name,
                family: workload.family.name(),
                row_count,
                rusthouse_samples_ms: rusthouse_samples,
                clickhouse_samples_ms: clickhouse_samples,
                rusthouse_median_ms: rusthouse_median,
                clickhouse_median_ms: clickhouse_median,
                ratio,
            });
        }
    }

    let ratios = cases.iter().map(|case| case.ratio).collect::<Vec<_>>();
    let score = parity_score(&ratios)?;
    if let Some(path) = &config.details {
        let details = details_json(&config, &identity, &cases, score, correctness_checks);
        fs::write(path, details)
            .map_err(|error| format!("could not write details to '{}': {error}", path.display()))?;
    }

    let mut evidence = vec![
        format!(
            "{} correctness-gated process pairs passed across {} cases and {} row counts",
            correctness_checks,
            cases.len(),
            settings.row_counts.len()
        ),
        format!(
            "mode={}, seed={}, warmups={}, samples={}; ClickHouse SHA-256={}",
            config.mode.name(),
            config.seed,
            settings.warmups,
            settings.samples,
            identity.sha256
        ),
        format!("ClickHouse identity: {}", identity.version_output),
    ];
    evidence.extend(cases.iter().map(|case| {
        format!(
            "{} / {} rows: RustHouse {:.3} ms, ClickHouse {:.3} ms, ClickHouse/RustHouse {:.3}",
            case.workload,
            case.row_count,
            case.rusthouse_median_ms,
            case.clickhouse_median_ms,
            case.ratio
        )
    }));
    let suggestions = if config.mode == config::Mode::Quick {
        vec![
            "Use --mode default for the decision-grade 1k/10k/50k-row suite.".to_owned(),
            "Rerun with several explicit --seed values before drawing optimization conclusions."
                .to_owned(),
        ]
    } else {
        vec![
            "Repeat the default suite with several explicit seeds and compare detailed case medians."
                .to_owned(),
            "Treat regressions in correctness as score zero, regardless of timing improvements."
                .to_owned(),
        ]
    };

    Ok(Report {
        score,
        summary: format!(
            "RustHouse scored {:.2} against ClickHouse Local parity=100 over {} correctness-gated cases.",
            score,
            cases.len()
        ),
        evidence,
        suggestions,
    })
}

fn execute_pair(
    paths: &EnginePaths,
    setup_sql: &str,
    query_sql: &str,
    rusthouse_first: bool,
) -> Result<(TimedOutput, TimedOutput), String> {
    if rusthouse_first {
        let rusthouse = paths.execute(Engine::RustHouse, setup_sql, query_sql)?;
        let clickhouse = paths.execute(Engine::ClickHouse, setup_sql, query_sql)?;
        Ok((rusthouse, clickhouse))
    } else {
        let clickhouse = paths.execute(Engine::ClickHouse, setup_sql, query_sql)?;
        let rusthouse = paths.execute(Engine::RustHouse, setup_sql, query_sql)?;
        Ok((rusthouse, clickhouse))
    }
}

fn accept_sample(
    columns: &[(&str, ColumnType)],
    rusthouse: &TimedOutput,
    clickhouse: &TimedOutput,
    record: bool,
    rusthouse_samples_ms: &mut Vec<f64>,
    clickhouse_samples_ms: &mut Vec<f64>,
) -> Result<(), String> {
    compare_outputs(&rusthouse.stdout, &clickhouse.stdout, columns)?;
    if record {
        rusthouse_samples_ms.push(rusthouse.elapsed.as_secs_f64() * 1_000.0);
        clickhouse_samples_ms.push(clickhouse.elapsed.as_secs_f64() * 1_000.0);
    }
    Ok(())
}

fn details_json(
    config: &Config,
    identity: &ClickHouseIdentity,
    cases: &[CaseResult],
    score: f64,
    correctness_checks: usize,
) -> String {
    let settings = config.mode.settings();
    let mut output = String::new();
    write!(
        output,
        "{{\"schema_version\":1,\"score\":{score:.6},\"mode\":{},\"seed\":{},\"warmups\":{},\"samples\":{},\"row_counts\":[",
        json_string(config.mode.name()),
        config.seed,
        settings.warmups,
        settings.samples
    )
    .expect("writing to String cannot fail");
    for (index, row_count) in settings.row_counts.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(output, "{row_count}").expect("writing to String cannot fail");
    }
    write!(
        output,
        "],\"correctness_checks\":{correctness_checks},\"rusthouse_path\":{},\"clickhouse_path\":{},\"clickhouse_version\":{},\"clickhouse_sha256\":{},\"cases\":[",
        json_string(&config.rusthouse.display().to_string()),
        json_string(&config.clickhouse.display().to_string()),
        json_string(&identity.version_output),
        json_string(&identity.sha256)
    )
    .expect("writing to String cannot fail");

    for (index, case) in cases.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(
            output,
            "{{\"workload\":{},\"family\":{},\"row_count\":{},\"rusthouse_median_ms\":{:.6},\"clickhouse_median_ms\":{:.6},\"clickhouse_rusthouse_ratio\":{:.9},\"rusthouse_samples_ms\":",
            json_string(case.workload),
            json_string(case.family),
            case.row_count,
            case.rusthouse_median_ms,
            case.clickhouse_median_ms,
            case.ratio
        )
        .expect("writing to String cannot fail");
        write_number_array(&mut output, &case.rusthouse_samples_ms);
        output.push_str(",\"clickhouse_samples_ms\":");
        write_number_array(&mut output, &case.clickhouse_samples_ms);
        output.push('}');
    }
    output.push_str("]}\n");
    output
}

fn write_number_array(output: &mut String, values: &[f64]) {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(output, "{value:.6}").expect("writing to String cannot fail");
    }
    output.push(']');
}

impl Report {
    fn to_json(&self) -> String {
        let mut output = format!(
            "{{\"score\":{:.6},\"summary\":{},\"evidence\":[",
            self.score,
            json_string(&self.summary)
        );
        write_string_array(&mut output, &self.evidence);
        output.push_str("],\"suggestions\":[");
        write_string_array(&mut output, &self.suggestions);
        output.push_str("]}");
        output
    }
}

fn write_string_array(output: &mut String, values: &[String]) {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&json_string(value));
    }
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => {
                write!(output, "\\u{:04x}", value as u32).expect("writing to String cannot fail");
            }
            value => output.push(value),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(csv: &str, milliseconds: u64) -> TimedOutput {
        TimedOutput {
            elapsed: Duration::from_millis(milliseconds),
            stdout: csv.to_owned(),
        }
    }

    #[test]
    fn correctness_gate_never_accepts_mismatched_timing() {
        let mut rusthouse_samples = Vec::new();
        let mut clickhouse_samples = Vec::new();
        let error = accept_sample(
            &[("n", ColumnType::Integer)],
            &output("n\n1\n", 10),
            &output("n\n2\n", 5),
            true,
            &mut rusthouse_samples,
            &mut clickhouse_samples,
        )
        .expect_err("mismatch must fail");

        assert!(error.contains("result mismatch"));
        assert!(rusthouse_samples.is_empty());
        assert!(clickhouse_samples.is_empty());
    }

    #[test]
    fn correctness_gate_accepts_only_normalized_matches() {
        let mut rusthouse_samples = Vec::new();
        let mut clickhouse_samples = Vec::new();
        accept_sample(
            &[("enabled", ColumnType::Boolean)],
            &output("enabled\ntrue\n", 10),
            &output("enabled\n1\n", 5),
            true,
            &mut rusthouse_samples,
            &mut clickhouse_samples,
        )
        .expect("matching output");

        assert_eq!(rusthouse_samples, [10.0]);
        assert_eq!(clickhouse_samples, [5.0]);
    }

    #[test]
    fn burner_report_is_one_compact_object_with_required_fields() {
        let report = Report {
            score: 10.0,
            summary: "summary".to_owned(),
            evidence: vec!["evidence".to_owned()],
            suggestions: vec!["suggestion".to_owned()],
        }
        .to_json();

        assert_eq!(
            report,
            "{\"score\":10.000000,\"summary\":\"summary\",\"evidence\":[\"evidence\"],\"suggestions\":[\"suggestion\"]}"
        );
        assert!(!report.contains('\n'));
    }
}
