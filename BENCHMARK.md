# ClickHouse parity benchmark

This repository ships a neutral, black-box benchmark in the benchmark directory. It compares the prebuilt rusthouse CLI with ClickHouse Local through child processes and does not call RustHouse engine modules. The benchmark is a measuring instrument, not an engine optimization.

## Pinned reference

The accepted reference is the supplied ClickHouse Local build:

- Version: ClickHouse local version 26.7.1.1315 (official build).
- SHA-256: 6611c5aadcfac188031fa0fdf2676ec311771f96654a62b918b146b60dd11075
- Binary size used for validation: 853,099,511 bytes

The harness executes the local version command, calculates SHA-256 with shasum, and fails before benchmarking if either the 26.7.1 version or pinned checksum differs. It also records the RustHouse binary SHA-256, source commit, inferred build profile, RUSTFLAGS, harness SHA-256, OS, CPU, and Rust toolchain. The ClickHouse executable is an external validation tool and must not be committed.

Verify it independently:

~~~bash
shasum -a 256 /path/to/clickhouse
/path/to/clickhouse local --version
~~~

## Exact commands

Build both shipped binaries first so compilation is never included:

~~~bash
cargo build --release --bins
~~~

A quick local check uses one runtime seed across three schema profiles and two row counts: 48 correctness-gated cases, one amplified warmup, three amplified samples, and three end-to-end samples.

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --quick \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-quick.json
~~~

The decision-grade default deterministically expands the runtime root into three seeds and uses all three profiles at 1,000, 10,000, and 50,000 rows: 216 correctness-gated cases, two amplified warmups, seven amplified samples, and three end-to-end samples.

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --mode default \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-default.json
~~~

## Historical grouping and top-k measurement

The following numbers predate schema profiles and used the retired single-schema, 24-case suite. They are retained as optimization history, not as a baseline comparable to current schema-version 3 benchmark output.

On 2026-07-29, the then-current single-schema default command was run on the same Apple Silicon host before and after replacing owned tree-based grouping and fully materialized sorting with borrowed hash grouping, columnar aggregate state, and index-based top-k execution. The baseline was commit `659c30b`; both runs used seed `20260729`, release binaries, the pinned ClickHouse build, and passed all 24 correctness gates. Times are RustHouse's seven-sample sustained per-query medians; the ratio is ClickHouse median divided by the optimized RustHouse median.

| Case | Rows | Before (ms) | After (ms) | Speedup | After ratio |
| --- | ---: | ---: | ---: | ---: | ---: |
| High-cardinality group by | 10,000 | 2.652 | 0.882 | 3.01x | 1.610 |
| High-cardinality group by | 50,000 | 15.124 | 4.317 | 3.50x | 0.952 |
| Numeric order by limit | 10,000 | 1.077 | 0.251 | 4.29x | 5.018 |
| Numeric order by limit | 50,000 | 6.458 | 1.096 | 5.89x | 2.935 |
| String order by limit | 10,000 | 1.769 | 0.442 | 4.00x | 2.685 |
| String order by limit | 50,000 | 11.596 | 1.985 | 5.84x | 1.279 |

The sustained score moved from 84.74 to 99.77; the startup-inclusive score was 100.00 in both runs. A second full default run with seed `20260730` passed 24/24 gates and scored 99.87. Its 50,000-row RustHouse medians were 4.271 ms for high-cardinality grouping, 1.123 ms for numeric ordering, and 1.978 ms for string ordering.

The --clickhouse flag is equivalent to RUSTHOUSE_CLICKHOUSE_BIN. The harness normally finds the prebuilt rusthouse next to itself; --rusthouse or RUSTHOUSE_BIN can override that path. In quick mode, --seed is the one runtime seed. In default mode it is the root of a three-value panel formed by wrapping additions of the SplitMix64 golden-ratio increment. Changing the root deterministically changes every panel value, profile, and row count's data.

Progress is written to stderr. Stdout is exactly one compact Burner JSON object with score, summary, evidence, and suggestions. Its score is the primary sustained-work score; summary and evidence also name the end-to-end score. Timed stdout is streamed through SHA-256 without being retained; every engine execution must match the exact byte count and digest produced by repeating that engine's correctness-gated single-query output. The --details option writes schema-versioned JSON containing the profiles, aggregation contract, fixed work budget, per-case amplification, correctness and timed-output check counts, output digests, raw batch and per-query samples, medians, both ratios and scores, paths, seed, mode, derived dataset seeds, and complete run provenance. Setup, execution, version, checksum, parse, correctness, output-digest, timing-stability, incomplete score-matrix, or full default-suite saturation failures still emit the one object with score zero and exit nonzero.

## Dataset and workloads

A dependency-free SplitMix64 generator produces deterministic typed source rows. Profile-specific salts and each selected seed produce three scored table shapes:

| Profile | Physical shape | Projection signal |
| --- | --- | --- |
| Numeric-heavy | Seven Int64, two Float64, one Bool, one String | 11-column point projection and broad numeric aggregates |
| String-heavy | Seven String, two Int64, one Float64, one Bool | 11-column point projection, string extrema, predicates, grouping, and ordering |
| Wide mixed | Seven Int64, two Float64, seven String, two Bool | 18-column point projection and 10-column mixed ordering projections |

Across those materialized shapes, source rows include:

- a broad uniform integer and a 90%-near-zero skewed integer;
- eight low-cardinality string keys and unique high-cardinality keys;
- variable-length strings, including commas and SQL quotes;
- both Boolean values;
- negative numbers and signed integers around four quadrillion;
- exactly representable eighth-step floating-point values.

The first rows force important extrema, so even quick mode cannot randomly omit negative, positive, or large values. Profile- and row-count-specific seed derivation prevents different shapes and larger sizes from merely timing the same data prefix.

Each profile and row count runs eight cases spanning:

| Family | Coverage |
| --- | --- |
| Full scan | Numeric aggregates, string extrema, and wide mixed aggregate projections |
| Selective filter | A single-ID predicate projecting all 11 or 18 physical columns |
| Compound filter | Profile-specific numeric, string, Boolean, and mixed AND/OR predicates |
| Nonselective filter | Numeric or string predicates designed to retain most rows |
| Low-cardinality grouping | Numeric buckets, string keys, and mixed Boolean dimensions |
| High-cardinality grouping | Unique numeric or string keys, deterministic ordering, bounded output |
| Ordering and limit | Two profile-specific numeric, string, or wide mixed sort shapes |

The generated CREATE TABLE, INSERT, and query SQL bytes are identical for both engines. Only public output-format command-line options differ. All result-producing queries have explicit aliases and deterministic ordering where row order matters.

## Correctness gate and normalization

Correctness and timing use separate processes. Before any timing for a case, the harness runs setup plus one unamplified query on each engine, captures both outputs, and opens that case's timing gate only after normalization succeeds. Amplified and end-to-end sample acceptance both require the open gate. Any process or comparison failure rejects the entire run; failed or absent gates cannot contribute timings.

The normalizer parses standards-compliant CSV, validates exact column names and widths, and compares values using declared workload types. Integers and strings remain exact. Boolean word and numeric spellings normalize to the same value. Finite floats use a relative tolerance of 1e-9 solely for rendering and accumulation-order noise. It does not sort results, discard columns, coerce strings, or accept malformed output.

Tests cover per-profile reproducibility, runtime-seed variation, physical schema shapes, execution of every profile query, projection metadata, CSV normalization, separate correctness gating, equal engine amplification, exact fixed-budget allocation, positive amortized timings, unstable-sample rejection, score saturation detection, equal profile/seed/family/scale weighting, and fail-closed matrix validation.

## Timing and calibration

The primary sample starts one process, creates and inserts one profile dataset, and executes an identical workload query repeatedly against that in-memory table. A fixed budget of 256 repeated queries per workload/scale/sample is divided across every profile/seed cell. Quick mode allocates 86, 85, and 85 repetitions across its three profile cells. Default mode allocates 29 repetitions to four cells and 28 to five cells across its nine profile/seed cells. The remainder rotates by workload and scale. This preserves the former suite's total sustained query work instead of multiplying it when profiles and seeds are added.

For each case, both engines receive the exact same setup SQL, query SQL, and repetition count; the runner rejects a mismatch. Stdout goes to the null sink for timing processes. Total positive wall time is divided by the case's exact repetition count, so startup and setup remain present at 1/85 or 1/86 in quick mode and 1/28 or 1/29 in default mode. Warmup process pairs are discarded, engine order alternates, and the median of measured samples is used.

The fixed 256-query cross-profile/seed budget is the calibration for both quick and default modes. There is no engine-dependent adaptive stopping. The harness deliberately performs no startup subtraction: subtracting independently noisy process measurements can create zero, negative, or highly unstable derived timings. Samples must remain positive, and a greater-than-10x max/min spread rejects the run.

A separate end-to-end metric times fresh processes containing setup plus one query. It uses three samples per case and includes startup, SQL parsing, table creation, insertion, execution, formatting, and process shutdown. This preserves the real CLI lifecycle signal instead of silently discarding it.

## Score aggregation

Each case ratio is ClickHouse median divided by RustHouse median. Ratios below 0.01 are floored and ratios above 1 are capped at parity before aggregation, so one unusually favorable RustHouse case cannot compensate for a slow family.

Before aggregation, the scorer requires the exact expected profile, seed, family, scale, and workload matrix. Missing, duplicate, or unexpected cases reject the whole run instead of silently changing weights. Aggregation is hierarchical in log space:

1. Workloads receive equal weight within each profile, seed, family, and scale.
2. Scales receive equal weight within each profile, seed, and family.
3. Families receive equal weight within each profile and seed.
4. Seeds receive equal weight within each profile.
5. Schema profiles receive equal weight in the final geometric mean.

The same aggregation produces primary and end-to-end scores. A ratio of one maps to 100, while a uniform ratio of 0.1 maps to 10. The decision-grade default rejects a result if every primary case reaches the 100 cap because that indicates no useful optimization headroom was measured. Quick mode reports its cap count without rejecting because its deliberately tiny scales can legitimately favor a minimal in-memory engine.

## Fairness, limitations, and anti-gaming

Amplification measures repeated work on one loaded in-memory table. It can benefit CPU caches and repeated planning paths, does not model concurrency, and retains the startup/setup fractions described above. The separately reported startup-inclusive score must be consulted for one-shot CLI use. Neither metric isolates only an execution kernel. Adding profiles and seeds increases correctness, setup, and fresh-process coverage even though the amplified sustained-query budget remains fixed.

OS scheduling, filesystem cache state, CPU frequency, and other local load remain uncontrolled. Synthetic data cannot represent production compression, joins, nullability, durable storage, network access, or concurrent clients, and this benchmark makes no such claim.

Anti-gaming properties are the fixed external ClickHouse identity, configurable runtime seeds, multiple scales, three deliberately conflicting physical schemas, selective and nonselective predicates, two grouping cardinalities, varied projection widths, deterministic query ordering, alternating engine order, separate fail-closed correctness gates, byte-identical per-engine SQL, symmetric amplification, retained raw samples, conservative per-case caps, equal profile/seed/family/scale weighting, and exact matrix validation. No single special-case query, favorable seed, duplicated workload, or omitted profile can legitimately stand in for the suite.
