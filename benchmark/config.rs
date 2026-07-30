use std::path::PathBuf;

pub const DEFAULT_SEED: u64 = 20_260_729;
pub const AUDIT_SEED_PANEL: [u64; 3] = [20_260_729, 20_260_730, 20_260_731];

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedSelection {
    Single(u64),
    AuditPanel,
}

impl SeedSelection {
    pub fn values(self) -> Vec<u64> {
        match self {
            Self::Single(seed) => vec![seed],
            Self::AuditPanel => AUDIT_SEED_PANEL.to_vec(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Single(_) => "single",
            Self::AuditPanel => "audit_panel",
        }
    }

    pub fn single_seed(self) -> Option<u64> {
        match self {
            Self::Single(seed) => Some(seed),
            Self::AuditPanel => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub seed_selection: SeedSelection,
    pub rusthouse: PathBuf,
    pub clickhouse: PathBuf,
    pub details: Option<PathBuf>,
}

pub enum ParseResult {
    Run(Config),
    Help,
}

#[derive(Debug)]
pub struct ParseFailure {
    message: String,
    details: Option<PathBuf>,
}

impl ParseFailure {
    pub fn into_parts(self) -> (String, Option<PathBuf>) {
        (self.message, self.details)
    }
}

impl std::fmt::Display for ParseFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub fn parse(
    arguments: impl IntoIterator<Item = String>,
    clickhouse_from_env: Option<String>,
    rusthouse_from_env: Option<String>,
    default_rusthouse: PathBuf,
) -> Result<ParseResult, ParseFailure> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let failure_details = prescan_details(&arguments);
    let mut mode = Mode::Default;
    let mut seed = None;
    let mut audit_seeds = false;
    let mut clickhouse = clickhouse_from_env.map(PathBuf::from);
    let mut rusthouse = rusthouse_from_env
        .map(PathBuf::from)
        .unwrap_or(default_rusthouse);
    let mut details = None;
    let mut arguments = arguments.into_iter();

    let result = (|| -> Result<ParseResult, String> {
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
                    reject_seed_conflict(audit_seeds)?;
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--seed requires an unsigned integer".to_owned())?;
                    seed = Some(parse_seed(&value)?);
                }
                "--seeds" => {
                    if seed.is_some() {
                        return Err("--seed and --seeds are mutually exclusive".to_owned());
                    }
                    if audit_seeds {
                        return Err("--seeds may only be supplied once".to_owned());
                    }
                    audit_seeds = true;
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
                    reject_seed_conflict(audit_seeds)?;
                    seed = Some(parse_seed(&argument["--seed=".len()..])?);
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
            "ClickHouse path is required; use --clickhouse PATH or RUSTHOUSE_CLICKHOUSE_BIN"
                .to_owned()
        })?;
        let seed_selection = if audit_seeds {
            SeedSelection::AuditPanel
        } else {
            SeedSelection::Single(seed.unwrap_or(DEFAULT_SEED))
        };
        Ok(ParseResult::Run(Config {
            mode,
            seed_selection,
            rusthouse,
            clickhouse,
            details: details.clone(),
        }))
    })();

    result.map_err(|message| ParseFailure {
        message,
        details: failure_details,
    })
}

fn prescan_details(arguments: &[String]) -> Option<PathBuf> {
    let mut details = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--details" => {
                if let Some(path) = arguments.get(index + 1) {
                    details = Some(PathBuf::from(path));
                    index += 1;
                }
            }
            "--mode" | "--seed" | "--clickhouse" | "--rusthouse" => {
                index += usize::from(arguments.get(index + 1).is_some());
            }
            _ if argument.starts_with("--details=") => {
                details = Some(PathBuf::from(&argument["--details=".len()..]));
            }
            _ => {}
        }
        index += 1;
    }
    details
}

fn reject_seed_conflict(audit_seeds: bool) -> Result<(), String> {
    if audit_seeds {
        Err("--seed and --seeds are mutually exclusive".to_owned())
    } else {
        Ok(())
    }
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
        assert_eq!(config.seed_selection, SeedSelection::Single(99));
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
    fn audit_flag_selects_the_documented_seed_panel() {
        let ParseResult::Run(config) = parse(
            ["--seeds", "--clickhouse=/clickhouse"]
                .into_iter()
                .map(str::to_owned),
            None,
            None,
            PathBuf::from("/rusthouse"),
        )
        .expect("configuration") else {
            panic!("expected run");
        };

        assert_eq!(config.seed_selection, SeedSelection::AuditPanel);
        assert_eq!(config.seed_selection.values(), AUDIT_SEED_PANEL);
    }

    #[test]
    fn single_seed_and_audit_panel_are_mutually_exclusive() {
        for arguments in [
            vec!["--seed=1", "--seeds", "--clickhouse=/clickhouse"],
            vec!["--seeds", "--seed=1", "--clickhouse=/clickhouse"],
        ] {
            let error = match parse(
                arguments.into_iter().map(str::to_owned),
                None,
                None,
                PathBuf::from("/rusthouse"),
            ) {
                Ok(_) => panic!("conflicting seed options should fail"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("mutually exclusive"));
        }
    }

    #[test]
    fn details_prescan_continues_after_errors_and_respects_option_arity() {
        let arguments = [
            "--seeds",
            "--seed=1",
            "--details",
            "audit.json",
            "--clickhouse=/clickhouse",
        ]
        .map(str::to_owned);
        assert_eq!(
            prescan_details(&arguments),
            Some(PathBuf::from("audit.json"))
        );

        let consumed_as_value = ["--rusthouse", "--details", "unknown"].map(str::to_owned);
        assert_eq!(prescan_details(&consumed_as_value), None);
    }

    #[test]
    fn clickhouse_path_is_required() {
        let error = match parse(std::iter::empty(), None, None, PathBuf::from("rusthouse")) {
            Ok(_) => panic!("missing ClickHouse path should fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("RUSTHOUSE_CLICKHOUSE_BIN"));
    }
}
