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
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(ParseResult::Help),
            "--quick" => mode = Mode::Quick,
            "--mode" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--mode requires quick or default".to_owned())?;
                mode = parse_mode(&value)?;
            }
            "--seed" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--seed requires an unsigned integer".to_owned())?;
                seed = parse_seed(&value)?;
            }
            "--clickhouse" => {
                clickhouse = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--clickhouse requires a path".to_owned())?,
                ));
            }
            "--rusthouse" => {
                rusthouse = PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--rusthouse requires a path".to_owned())?,
                );
            }
            "--details" => {
                details = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--details requires a path".to_owned())?,
                ));
            }
            _ if argument.starts_with("--mode=") => {
                mode = parse_mode(&argument["--mode=".len()..])?;
            }
            _ if argument.starts_with("--seed=") => {
                seed = parse_seed(&argument["--seed=".len()..])?;
            }
            _ if argument.starts_with("--clickhouse=") => {
                clickhouse = Some(PathBuf::from(&argument["--clickhouse=".len()..]));
            }
            _ if argument.starts_with("--rusthouse=") => {
                rusthouse = PathBuf::from(&argument["--rusthouse=".len()..]);
            }
            _ if argument.starts_with("--details=") => {
                details = Some(PathBuf::from(&argument["--details=".len()..]));
            }
            _ => return Err(format!("unknown argument {argument:?}; try --help")),
        }
    }

    let clickhouse = clickhouse.ok_or_else(|| {
        "ClickHouse path is required; use --clickhouse PATH or RUSTHOUSE_CLICKHOUSE_BIN".to_owned()
    })?;
    if mode == Mode::Default && details.is_none() {
        return Err(
            "default mode requires --details PATH so the benchmark attestation is retained"
                .to_owned(),
        );
    }
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
    fn default_mode_requires_retained_details() {
        let error = match parse(
            ["--clickhouse=/clickhouse"].into_iter().map(str::to_owned),
            None,
            None,
            PathBuf::from("rusthouse"),
        ) {
            Ok(_) => panic!("default mode without details should fail"),
            Err(error) => error,
        };
        assert!(error.contains("requires --details"));

        let ParseResult::Run(config) = parse(
            ["--quick", "--clickhouse=/clickhouse"]
                .into_iter()
                .map(str::to_owned),
            None,
            None,
            PathBuf::from("rusthouse"),
        )
        .expect("quick mode may omit retained details") else {
            panic!("expected run");
        };
        assert_eq!(config.mode, Mode::Quick);
        assert!(config.details.is_none());
    }
}
