use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Quick,
    Default,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Default => "default",
        }
    }

    pub fn settings(self) -> BenchmarkSettings {
        match self {
            Self::Quick => BenchmarkSettings {
                row_counts: vec![256, 2_048],
                warmups: 1,
                samples: 3,
                query_amplification: 256,
                end_to_end_samples: 3,
            },
            Self::Default => BenchmarkSettings {
                row_counts: vec![1_000, 10_000, 50_000],
                warmups: 2,
                samples: 7,
                query_amplification: 256,
                end_to_end_samples: 3,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchmarkSettings {
    pub row_counts: Vec<usize>,
    pub warmups: usize,
    pub samples: usize,
    pub query_amplification: usize,
    pub end_to_end_samples: usize,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub seed: u64,
    pub rusthouse: PathBuf,
    pub clickhouse: PathBuf,
    pub details: Option<PathBuf>,
}

pub enum ParseResult {
    Run(Config),
    Verify(PathBuf),
    Help,
}

pub fn parse(
    arguments: impl IntoIterator<Item = String>,
    clickhouse_from_env: Option<String>,
    rusthouse_from_env: Option<String>,
    default_rusthouse: PathBuf,
) -> Result<ParseResult, String> {
    let mut mode = Mode::Default;
    let mut seed = 20_260_729_u64;
    let mut clickhouse = clickhouse_from_env.map(PathBuf::from);
    let mut rusthouse = rusthouse_from_env
        .map(PathBuf::from)
        .unwrap_or(default_rusthouse);
    let mut details = None;
    let mut verify_details = None;
    let mut benchmark_option_seen = false;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(ParseResult::Help),
            "--quick" => {
                benchmark_option_seen = true;
                mode = Mode::Quick;
            }
            "--mode" => {
                benchmark_option_seen = true;
                let value = arguments
                    .next()
                    .ok_or_else(|| "--mode requires quick or default".to_owned())?;
                mode = parse_mode(&value)?;
            }
            "--seed" => {
                benchmark_option_seen = true;
                let value = arguments
                    .next()
                    .ok_or_else(|| "--seed requires an unsigned integer".to_owned())?;
                seed = parse_seed(&value)?;
            }
            "--clickhouse" => {
                benchmark_option_seen = true;
                clickhouse = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--clickhouse requires a path".to_owned())?,
                ));
            }
            "--rusthouse" => {
                benchmark_option_seen = true;
                rusthouse = PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--rusthouse requires a path".to_owned())?,
                );
            }
            "--details" => {
                benchmark_option_seen = true;
                details = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--details requires a path".to_owned())?,
                ));
            }
            "--verify-details" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--verify-details requires a path".to_owned())?;
                if verify_details.replace(PathBuf::from(path)).is_some() {
                    return Err("--verify-details may only be specified once".to_owned());
                }
            }
            _ if argument.starts_with("--mode=") => {
                benchmark_option_seen = true;
                mode = parse_mode(&argument["--mode=".len()..])?;
            }
            _ if argument.starts_with("--seed=") => {
                benchmark_option_seen = true;
                seed = parse_seed(&argument["--seed=".len()..])?;
            }
            _ if argument.starts_with("--clickhouse=") => {
                benchmark_option_seen = true;
                clickhouse = Some(PathBuf::from(&argument["--clickhouse=".len()..]));
            }
            _ if argument.starts_with("--rusthouse=") => {
                benchmark_option_seen = true;
                rusthouse = PathBuf::from(&argument["--rusthouse=".len()..]);
            }
            _ if argument.starts_with("--details=") => {
                benchmark_option_seen = true;
                details = Some(PathBuf::from(&argument["--details=".len()..]));
            }
            _ if argument.starts_with("--verify-details=") => {
                let value = &argument["--verify-details=".len()..];
                if value.is_empty() {
                    return Err("--verify-details requires a path".to_owned());
                }
                if verify_details.replace(PathBuf::from(value)).is_some() {
                    return Err("--verify-details may only be specified once".to_owned());
                }
            }
            _ => return Err(format!("unknown argument {argument:?}; try --help")),
        }
    }

    if let Some(path) = verify_details {
        if benchmark_option_seen {
            return Err(
                "--verify-details cannot be combined with benchmark execution options".to_owned(),
            );
        }
        return Ok(ParseResult::Verify(path));
    }

    let clickhouse = clickhouse.ok_or_else(|| {
        "ClickHouse path is required; use --clickhouse PATH or RUSTHOUSE_CLICKHOUSE_BIN".to_owned()
    })?;
    Ok(ParseResult::Run(Config {
        mode,
        seed,
        rusthouse,
        clickhouse,
        details,
    }))
}

fn parse_mode(value: &str) -> Result<Mode, String> {
    match value {
        "quick" => Ok(Mode::Quick),
        "default" => Ok(Mode::Default),
        _ => Err(format!("unknown mode {value:?}; expected quick or default")),
    }
}

fn parse_seed(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("invalid seed {value:?}; expected an unsigned integer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_overrides_environment_and_accepts_runtime_seed() {
        let ParseResult::Run(config) = parse(
            [
                "--quick",
                "--seed=99",
                "--clickhouse=/command/clickhouse",
                "--rusthouse=/command/rusthouse",
                "--details=details.json",
            ]
            .into_iter()
            .map(str::to_owned),
            Some("/environment/clickhouse".to_owned()),
            None,
            PathBuf::from("/default/rusthouse"),
        )
        .expect("configuration") else {
            panic!("expected run");
        };

        assert_eq!(config.mode, Mode::Quick);
        assert_eq!(config.seed, 99);
        assert_eq!(config.clickhouse, PathBuf::from("/command/clickhouse"));
        assert_eq!(config.rusthouse, PathBuf::from("/command/rusthouse"));
        assert_eq!(config.details, Some(PathBuf::from("details.json")));
    }

    #[test]
    fn modes_use_multiple_row_counts_and_repeated_samples() {
        for mode in [Mode::Quick, Mode::Default] {
            let settings = mode.settings();
            assert!(settings.row_counts.len() >= 2);
            assert!(settings.warmups >= 1);
            assert!(settings.samples >= 3);
            assert!(settings.query_amplification > 1);
            assert!(settings.end_to_end_samples >= 3);
        }
    }

    #[test]
    fn clickhouse_path_is_required() {
        let error = match parse(std::iter::empty(), None, None, PathBuf::from("rusthouse")) {
            Ok(_) => panic!("missing ClickHouse path should fail"),
            Err(error) => error,
        };
        assert!(error.contains("RUSTHOUSE_CLICKHOUSE_BIN"));
    }

    #[test]
    fn verification_is_offline_and_exclusive() {
        let parsed = parse(
            ["--verify-details", "evidence.json"]
                .into_iter()
                .map(str::to_owned),
            None,
            None,
            PathBuf::from("/missing/rusthouse"),
        )
        .expect("offline verification configuration");
        assert!(matches!(
            parsed,
            ParseResult::Verify(path) if path.as_path() == std::path::Path::new("evidence.json")
        ));

        let error = match parse(
            ["--quick", "--verify-details=evidence.json"]
                .into_iter()
                .map(str::to_owned),
            None,
            None,
            PathBuf::from("rusthouse"),
        ) {
            Ok(_) => panic!("verification and execution options should conflict"),
            Err(error) => error,
        };
        assert!(error.contains("cannot be combined"));
    }
}
