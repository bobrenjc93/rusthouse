use std::fs;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

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
    pub correctness_audit: bool,
    pub audit_sql: Option<PathBuf>,
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
    let mut correctness_audit = false;
    let mut audit_sql = None;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(ParseResult::Help),
            "--quick" => mode = Mode::Quick,
            "--correctness-audit" => correctness_audit = true,
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
            "--audit-sql" => {
                audit_sql = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--audit-sql requires a path".to_owned())?,
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
            _ if argument.starts_with("--audit-sql=") => {
                audit_sql = Some(PathBuf::from(&argument["--audit-sql=".len()..]));
            }
            _ => return Err(format!("unknown argument {argument:?}; try --help")),
        }
    }

    let clickhouse = clickhouse.ok_or_else(|| {
        "ClickHouse path is required; use --clickhouse PATH or RUSTHOUSE_CLICKHOUSE_BIN".to_owned()
    })?;
    if audit_sql.is_some() && !correctness_audit {
        return Err("--audit-sql requires --correctness-audit".to_owned());
    }
    if correctness_audit {
        let audit_destination = audit_sql
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("clickhouse-correctness-audit-{seed}.sql")));
        if let Some(details) = &details
            && destinations_collide(details, &audit_destination)
        {
            return Err(format!(
                "details output '{}' and correctness-audit replay SQL '{}' must use distinct paths",
                details.display(),
                audit_destination.display(),
            ));
        }
    }
    Ok(ParseResult::Run(Config {
        mode,
        seed,
        rusthouse,
        clickhouse,
        details,
        correctness_audit,
        audit_sql,
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

fn destinations_collide(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    if let (Some(left), Some(right)) = (resolved_destination(left), resolved_destination(right))
        && left == right
    {
        return true;
    }
    same_existing_file(left, right)
}

fn resolved_destination(path: &Path) -> Option<PathBuf> {
    let mut destination = canonical_parent_destination(path).or_else(|| lexical_absolute(path))?;
    for _ in 0..40 {
        let target = match fs::read_link(&destination) {
            Ok(target) => target,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
                ) =>
            {
                return Some(destination);
            }
            Err(_) => return None,
        };
        let target = if target.is_absolute() {
            target
        } else {
            destination.parent()?.join(target)
        };
        destination =
            canonical_parent_destination(&target).or_else(|| lexical_absolute(&target))?;
    }
    None
}

fn lexical_absolute(path: &Path) -> Option<PathBuf> {
    let absolute = absolute_path(path)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    Some(normalized)
}

fn canonical_parent_destination(path: &Path) -> Option<PathBuf> {
    let absolute = absolute_path(path)?;
    let file_name = absolute.file_name()?.to_owned();
    let parent = fs::canonicalize(absolute.parent()?).ok()?;
    Some(parent.join(file_name))
}

fn absolute_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_owned())
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

#[cfg(unix)]
fn same_existing_file(left: &Path, right: &Path) -> bool {
    let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_existing_file(_left: &Path, _right: &Path) -> bool {
    false
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
                "--correctness-audit",
                "--audit-sql=audit.sql",
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
        assert!(config.correctness_audit);
        assert_eq!(config.audit_sql, Some(PathBuf::from("audit.sql")));
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
    fn audit_sql_requires_the_correctness_audit() {
        let error = match parse(
            [
                "--clickhouse=/clickhouse".to_owned(),
                "--audit-sql=audit.sql".to_owned(),
            ],
            None,
            None,
            PathBuf::from("rusthouse"),
        ) {
            Ok(_) => panic!("orphaned audit output should fail"),
            Err(error) => error,
        };
        assert!(error.contains("--correctness-audit"));
    }

    #[test]
    fn details_and_audit_sql_destinations_must_be_distinct() {
        for arguments in [
            vec![
                "--clickhouse=/clickhouse",
                "--correctness-audit",
                "--audit-sql=artifact",
                "--details=artifact",
            ],
            vec![
                "--clickhouse=/clickhouse",
                "--correctness-audit",
                "--audit-sql=output/../artifact",
                "--details=artifact",
            ],
            vec![
                "--clickhouse=/clickhouse",
                "--correctness-audit",
                "--seed=7",
                "--details=clickhouse-correctness-audit-7.sql",
            ],
        ] {
            let error = match parse(
                arguments.into_iter().map(str::to_owned),
                None,
                None,
                PathBuf::from("rusthouse"),
            ) {
                Ok(_) => panic!("colliding output destinations should fail"),
                Err(error) => error,
            };
            assert!(
                error.contains("distinct paths"),
                "unexpected error: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn destination_aliases_include_symlinks_and_hard_links() {
        use std::os::unix::fs::symlink;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rusthouse-output-aliases-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create alias test directory");

        let first_symlink = directory.join("first-symlink");
        let second_symlink = directory.join("second-symlink");
        symlink("future-output", &first_symlink).expect("create first dangling symlink");
        symlink("future-output", &second_symlink).expect("create second dangling symlink");
        assert!(destinations_collide(&first_symlink, &second_symlink));

        let file = directory.join("file");
        let hard_link = directory.join("hard-link");
        fs::write(&file, "content").expect("write hard-link source");
        fs::hard_link(&file, &hard_link).expect("create hard link");
        assert!(destinations_collide(&file, &hard_link));

        fs::remove_dir_all(&directory).expect("remove alias test directory");
    }
}
