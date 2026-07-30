use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const CLICKHOUSE_VERSION: &str = "26.7.1";
pub const CLICKHOUSE_SHA256: &str =
    "6611c5aadcfac188031fa0fdf2676ec311771f96654a62b918b146b60dd11075";

#[derive(Debug, Clone)]
pub struct EnginePaths {
    pub rusthouse: PathBuf,
    pub clickhouse: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ClickHouseIdentity {
    pub version_output: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Engine {
    RustHouse,
    ClickHouse,
}

#[derive(Debug)]
pub struct TimedOutput {
    pub stdout: String,
    pub query_repetitions: usize,
    pub batch_sha256: String,
}

#[derive(Debug)]
pub struct TimedBatch {
    pub elapsed: Duration,
    pub query_repetitions: usize,
    pub batch_sha256: String,
}

#[derive(Debug, Clone)]
pub struct SqlBatch {
    sql: String,
    query_repetitions: usize,
    sha256: String,
}

impl SqlBatch {
    pub fn new(setup_sql: &str, queries: &[&str]) -> Result<Self, String> {
        if queries.is_empty() {
            return Err("query repetition count must be positive".to_owned());
        }
        if queries.iter().any(|query| !query.trim_end().ends_with(';')) {
            return Err("every query variant must end with a semicolon".to_owned());
        }

        let query_bytes = queries.iter().try_fold(0_usize, |total, query| {
            total
                .checked_add(query.len())
                .and_then(|length| length.checked_add(1))
                .ok_or_else(|| "amplified SQL batch is too large".to_owned())
        })?;
        let capacity = setup_sql
            .len()
            .checked_add(query_bytes)
            .ok_or_else(|| "amplified SQL batch is too large".to_owned())?;
        let mut sql = String::with_capacity(capacity);
        sql.push_str(setup_sql);
        for query in queries {
            sql.push_str(query);
            sql.push('\n');
        }

        let sha256 = sha256_hex(sql.as_bytes());
        Ok(Self {
            sql,
            query_repetitions: queries.len(),
            sha256,
        })
    }

    pub fn query_repetitions(&self) -> usize {
        self.query_repetitions
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

impl EnginePaths {
    pub fn validate(&self) -> Result<ClickHouseIdentity, String> {
        validate_rusthouse(&self.rusthouse)?;
        validate_clickhouse(&self.clickhouse)
    }

    pub fn execute_correctness(
        &self,
        engine: Engine,
        batch: &SqlBatch,
    ) -> Result<TimedOutput, String> {
        let (_, stdout) = self.execute_batch(engine, &batch.sql, true)?;
        Ok(TimedOutput {
            stdout: stdout.expect("captured execution returns stdout"),
            query_repetitions: batch.query_repetitions,
            batch_sha256: batch.sha256.clone(),
        })
    }

    pub fn execute_timed(&self, engine: Engine, batch: &SqlBatch) -> Result<TimedBatch, String> {
        let (elapsed, stdout) = self.execute_batch(engine, &batch.sql, false)?;
        debug_assert!(stdout.is_none());
        Ok(TimedBatch {
            elapsed,
            query_repetitions: batch.query_repetitions,
            batch_sha256: batch.sha256.clone(),
        })
    }

    fn execute_batch(
        &self,
        engine: Engine,
        batch: &str,
        capture_stdout: bool,
    ) -> Result<(Duration, Option<String>), String> {
        let mut command = match engine {
            Engine::RustHouse => {
                let mut command = Command::new(&self.rusthouse);
                command.args(["--format", "csv"]);
                command
            }
            Engine::ClickHouse => {
                let mut command = Command::new(&self.clickhouse);
                command.args(["local", "--multiquery", "--output-format", "CSVWithNames"]);
                command
            }
        };
        command
            .stdin(Stdio::piped())
            .stdout(if capture_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped());

        let started = Instant::now();
        let mut child = command.spawn().map_err(|error| {
            format!(
                "could not start {} at '{}': {error}",
                engine.name(),
                engine.path(self).display()
            )
        })?;
        child
            .stdin
            .take()
            .ok_or_else(|| format!("{} stdin was not piped", engine.name()))?
            .write_all(batch.as_bytes())
            .map_err(|error| format!("could not write SQL to {}: {error}", engine.name()))?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("could not wait for {}: {error}", engine.name()))?;
        let elapsed = started.elapsed();

        if !output.status.success() {
            return Err(format!(
                "{} exited with {}: {}",
                engine.name(),
                output.status,
                summarize_stderr(&output.stderr)
            ));
        }
        let stdout =
            if capture_stdout {
                Some(String::from_utf8(output.stdout).map_err(|error| {
                    format!("{} emitted non-UTF-8 output: {error}", engine.name())
                })?)
            } else {
                None
            };
        Ok((elapsed, stdout))
    }
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Self::RustHouse => "RustHouse",
            Self::ClickHouse => "ClickHouse Local",
        }
    }

    fn path(self, paths: &EnginePaths) -> &Path {
        match self {
            Self::RustHouse => &paths.rusthouse,
            Self::ClickHouse => &paths.clickhouse,
        }
    }
}

fn validate_rusthouse(path: &Path) -> Result<(), String> {
    let output = Command::new(path).arg("--help").output().map_err(|error| {
        format!(
            "could not execute RustHouse at '{}': {error}",
            path.display()
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "RustHouse validation failed with {}: {}",
            output.status,
            summarize_stderr(&output.stderr)
        ));
    }
    Ok(())
}

fn validate_clickhouse(path: &Path) -> Result<ClickHouseIdentity, String> {
    let output = Command::new(path)
        .args(["local", "--version"])
        .output()
        .map_err(|error| {
            format!(
                "could not execute ClickHouse Local at '{}': {error}",
                path.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "ClickHouse version check failed with {}: {}",
            output.status,
            summarize_stderr(&output.stderr)
        ));
    }
    let version_output = String::from_utf8(output.stdout)
        .map_err(|error| format!("ClickHouse version output was not UTF-8: {error}"))?
        .trim()
        .to_owned();
    if !version_output.contains(CLICKHOUSE_VERSION) {
        return Err(format!(
            "unsupported ClickHouse version {version_output:?}; expected {CLICKHOUSE_VERSION}"
        ));
    }

    let checksum = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .map_err(|error| format!("could not calculate ClickHouse SHA-256: {error}"))?;
    if !checksum.status.success() {
        return Err(format!(
            "ClickHouse checksum failed with {}: {}",
            checksum.status,
            summarize_stderr(&checksum.stderr)
        ));
    }
    let checksum_output = String::from_utf8(checksum.stdout)
        .map_err(|error| format!("checksum output was not UTF-8: {error}"))?;
    let sha256 = checksum_output
        .split_whitespace()
        .next()
        .filter(|value| {
            value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit())
        })
        .ok_or_else(|| format!("unexpected shasum output: {checksum_output:?}"))?
        .to_ascii_lowercase();

    if sha256 != CLICKHOUSE_SHA256 {
        return Err(format!(
            "ClickHouse checksum mismatch: expected {CLICKHOUSE_SHA256}, got {sha256}"
        ));
    }

    Ok(ClickHouseIdentity {
        version_output,
        sha256,
    })
}

fn summarize_stderr(stderr: &[u8]) -> String {
    let rendered = String::from_utf8_lossy(stderr);
    let mut summary = rendered.trim().chars().take(2_000).collect::<String>();
    if rendered.trim().chars().count() > 2_000 {
        summary.push_str("...");
    }
    if summary.is_empty() {
        "<no stderr>".to_owned()
    } else {
        summary
    }
}

fn sha256_hex(input: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((input.len() + 72) & !63);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut state = INITIAL;
    let mut schedule = [0_u32; 64];
    for chunk in padded.chunks_exact(64) {
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let left = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let right = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(left)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(right);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let big_right = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temporary_one = h
                .wrapping_add(big_right)
                .wrapping_add(choice)
                .wrapping_add(ROUND[index])
                .wrapping_add(schedule[index]);
            let big_left = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary_two = big_left.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary_one);
            d = c;
            c = b;
            b = a;
            a = temporary_one.wrapping_add(temporary_two);
        }
        for (current, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *current = current.wrapping_add(value);
        }
    }

    state.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_preserves_order_and_counts_distinct_variants() {
        let batch = SqlBatch::new(
            "CREATE TABLE t (n Int64);\n",
            &[
                "SELECT n FROM t WHERE n = 1;",
                "SELECT n FROM t WHERE n = 2;",
            ],
        )
        .expect("valid batch");
        assert_eq!(batch.query_repetitions(), 2);
        assert_eq!(batch.sql.matches("CREATE TABLE").count(), 1);
        assert!(
            batch.sql.find("n = 1").expect("first variant")
                < batch.sql.find("n = 2").expect("second variant")
        );
    }

    #[test]
    fn amplification_must_be_positive() {
        assert!(SqlBatch::new("", &[]).is_err());
    }

    #[test]
    fn batch_digest_is_sha256_of_the_exact_engine_bytes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let first = SqlBatch::new("", &["SELECT 1;"]).expect("batch");
        let repeated = SqlBatch::new("", &["SELECT 1;"]).expect("batch");
        let changed = SqlBatch::new("", &["SELECT 2;"]).expect("batch");
        assert_eq!(first.sha256(), repeated.sha256());
        assert_ne!(first.sha256(), changed.sha256());
    }
}
