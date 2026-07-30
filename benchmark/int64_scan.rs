use std::hint::black_box;
use std::time::{Duration, Instant};

use rusthouse::storage::Int64Column;

const ROWS: usize = 4 * 1_024 * 1_024;
const REPETITIONS: usize = 4;
const SAMPLES: usize = 7;

fn main() {
    let cases = [
        ("constant", vec![17_i64; ROWS]),
        (
            "delta",
            (0..ROWS).map(|index| index as i64).collect::<Vec<_>>(),
        ),
        ("raw", random_values(0x5eed_cafe_f00d_beef, ROWS)),
    ];

    println!(
        "encoding,logical_bytes,encoded_bytes,raw_ns_per_value,chunked_ns_per_value,scan_ratio"
    );
    for (name, raw) in cases {
        let column = raw.iter().copied().collect::<Int64Column>();
        let expected = scan_raw(&raw, 1);
        assert_eq!(scan_chunked(&column, 1), expected, "{name} scan parity");

        scan_raw(black_box(&raw), 2);
        scan_chunked(black_box(&column), 2);
        let raw_duration = median_duration(|| scan_raw(black_box(&raw), REPETITIONS));
        let chunked_duration = median_duration(|| scan_chunked(black_box(&column), REPETITIONS));
        let values_scanned = (ROWS * REPETITIONS) as f64;
        let raw_ns = raw_duration.as_nanos() as f64 / values_scanned;
        let chunked_ns = chunked_duration.as_nanos() as f64 / values_scanned;
        let stats = column.storage_stats();

        println!(
            "{name},{},{},{raw_ns:.3},{chunked_ns:.3},{:.3}",
            stats.logical_bytes,
            stats.encoded_bytes,
            chunked_ns / raw_ns
        );
    }
}

fn median_duration(mut scan: impl FnMut() -> ScanDigest) -> Duration {
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        black_box(scan());
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    samples[SAMPLES / 2]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScanDigest {
    sum: i64,
    minimum: i64,
    maximum: i64,
    matched: usize,
}

impl ScanDigest {
    fn update(mut self, value: i64) -> Self {
        self.sum = self.sum.wrapping_add(value);
        self.minimum = self.minimum.min(value);
        self.maximum = self.maximum.max(value);
        self.matched += usize::from(value & 7 == 0);
        self
    }
}

impl Default for ScanDigest {
    fn default() -> Self {
        Self {
            sum: 0,
            minimum: i64::MAX,
            maximum: i64::MIN,
            matched: 0,
        }
    }
}

fn scan_raw(values: &[i64], repetitions: usize) -> ScanDigest {
    let mut digest = ScanDigest::default();
    for _ in 0..repetitions {
        digest = values
            .iter()
            .fold(digest, |digest, &value| digest.update(black_box(value)));
    }
    digest
}

fn scan_chunked(values: &Int64Column, repetitions: usize) -> ScanDigest {
    let mut digest = ScanDigest::default();
    for _ in 0..repetitions {
        digest = values
            .iter()
            .fold(digest, |digest, value| digest.update(black_box(value)));
    }
    digest
}

fn random_values(seed: u64, len: usize) -> Vec<i64> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = state;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (value ^ (value >> 31)) as i64
        })
        .collect()
}
