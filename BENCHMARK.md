# ClickHouse parity benchmark

This repository ships a neutral, black-box benchmark in the benchmark directory. It compares the prebuilt rusthouse CLI with ClickHouse Local through child processes and does not call RustHouse engine modules. The benchmark is a measuring instrument, not an engine optimization.

## Pinned reference

The accepted reference is the supplied ClickHouse Local build:

- Version: ClickHouse local version 26.7.1.1315 (official build).
- SHA-256: 6611c5aadcfac188031fa0fdf2676ec311771f96654a62b918b146b60dd11075
- Binary size used for validation: 853,099,511 bytes

The harness executes the local version command, calculates SHA-256 with shasum, and fails before benchmarking if either the 26.7.1 version or pinned checksum differs. The ClickHouse executable is an external validation tool and must not be committed.

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

A quick local check uses two row counts, one amplified warmup, three amplified samples, and three end-to-end samples:

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --quick \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-quick.json
~~~

The decision-grade default uses a fixed target of 16,000,000 row visits per amplified batch, with two amplified warmups, seven amplified samples, and three end-to-end samples per case. Repetition counts are derived as `target row visits / rows`, not selected from either engine's timings:

| Rows | Query amplification | Target row visits |
| ---: | ---: | ---: |
| 100,000 | 160x | 16,000,000 |
| 1,000,000 | 16x | 16,000,000 |

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --mode default \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-default.json
~~~

The earlier 1,000/10,000/50,000-row default capped 23 of 24 primary cases. Those historical scores are not comparable to this fixed-work, larger-scale baseline.

On 2026-07-30, the default command above passed all 26 result-parity gates against the pinned ClickHouse build. It scored 86.97 for sustained work and 97.24 for the separately retained startup-inclusive metric; 12 of 26 primary cases reached the parity cap, below the strict majority rejection threshold. These host-specific results document calibration rather than promise performance on other systems.

The --clickhouse flag is equivalent to RUSTHOUSE_CLICKHOUSE_BIN. The harness normally finds the prebuilt rusthouse next to itself; --rusthouse or RUSTHOUSE_BIN can override that path. A runtime --seed value deterministically changes every row count's data.

Progress is written to stderr. Stdout is exactly one compact Burner JSON object with score, summary, evidence, and suggestions. Its score is the primary sustained-work score; summary and evidence also name the separate startup-inclusive score. The --details option writes schema-versioned JSON containing acceptance status, the target row-visit budget and derived scale matrix, timing method and limitations, correctness count, raw batch and per-query samples, medians, both ratios and scores, paths, seed, mode, and ClickHouse identity. A saturation-rejected default still writes these details when requested. Setup, execution, version, checksum, parse, correctness, timing-stability, or excessive-saturation failures still emit the one object with score zero and exit nonzero.

## Dataset and workloads

A dependency-free SplitMix64 generator produces deterministic typed rows. Every dataset has:

- a broad uniform integer and a 90%-near-zero skewed integer;
- eight low-cardinality string keys, 1,024 deterministic medium-cardinality keys, and unique high-cardinality keys;
- variable-length strings, including commas and SQL quotes;
- both Boolean values;
- negative numbers and signed integers around four quadrillion;
- exactly representable eighth-step floating-point values.

The first rows force important extrema, so even quick mode cannot randomly omit negative, positive, or large values. Row-count-specific seed derivation prevents the larger sizes from merely timing the same prefix.

Each row count runs thirteen cases spanning nine score-balanced families:

| Family | Coverage |
| --- | --- |
| Full scan | COUNT plus integer and Float64 SUM, MIN, MAX, and AVG |
| Selective filter | A single-ID point predicate plus numeric predicates retaining about 10% and 50% |
| Compound filter | Parenthesized AND/OR, Boolean, uniform, and skewed columns |
| Nonselective filter | A predicate expected to retain about 97.5% of rows |
| String filter | Medium-key equality with a complete, non-LIMIT projection and a bounded range retaining about 10% |
| Low-cardinality grouping | String plus Boolean grouping with several aggregates |
| Medium-cardinality grouping | Deterministic 1,024-way string grouping with integer and Float64 aggregates |
| High-cardinality grouping | Unique string-key grouping, deterministic ordering, bounded output |
| Ordering and limit | Numeric and string sort shapes with deterministic tie breakers |

The generated CREATE TABLE, INSERT, and query SQL bytes are identical for both engines. Only public output-format command-line options differ. All result-producing queries have explicit aliases and deterministic ordering where row order matters.

## Correctness gate and normalization

Correctness and timing use separate processes. Before any timing for a case, the harness runs setup plus one unamplified query on each engine, captures both outputs, and opens that case's timing gate only after normalization succeeds. Amplified and end-to-end sample acceptance both require the open gate. Any process or comparison failure rejects the entire run; failed or absent gates cannot contribute timings.

The normalizer parses standards-compliant CSV, validates exact column names and widths, and compares values using declared workload types. Integers and strings remain exact. Boolean word and numeric spellings normalize to the same value. Finite floats use a relative tolerance of 1e-9 solely for rendering and accumulation-order noise. It does not sort results, discard columns, coerce strings, or accept malformed output.

Tests cover generator reproducibility, runtime-seed variation, dataset-shape, selectivity, workload-diversity, and equal-work scale invariants, CSV normalization, separate correctness gating, equal engine amplification, positive amortized timings, unstable-sample rejection, the 50% saturation boundary, retained details, and family/scale weighting.

## Timing and calibration

The primary sample starts one process, creates and inserts the dataset once, and executes the identical workload query using the scale's budget-derived amplification: 160 times at 100,000 rows and 16 times at 1,000,000 rows. Both engines receive exactly the same repetitions for a case; the runner rejects a mismatched count. Stdout goes to the null sink for timing processes. Total positive wall time is divided by the repetition count, so startup and setup contribute 1/160 or 1/16 to the reported per-query sample. Warmup process pairs are discarded, engine order alternates, and the median of measured samples is used.

The 16,000,000-row target is fixed in source before either engine runs, and the selected decade scales divide it exactly. This gives each default case the same nominal scan work without engine-dependent adaptive stopping or result-dependent calibration. Quick mode retains its fixed 256x calibration. The harness deliberately performs no startup subtraction: subtracting independently noisy process measurements can create zero, negative, or highly unstable derived timings. Samples must remain positive, and a greater-than-10x max/min spread rejects the run.

A separate end-to-end metric times fresh processes containing setup plus one query. It uses three samples per case and includes startup, SQL parsing, table creation, insertion, execution, formatting, and process shutdown. This preserves the real CLI lifecycle signal instead of silently discarding it.

## Score aggregation

Each case ratio is ClickHouse median divided by RustHouse median. Ratios below 0.01 are floored and ratios above 1 are capped at parity before aggregation, so one unusually favorable RustHouse case cannot compensate for a slow family.

Aggregation is hierarchical in log space:

1. Workloads receive equal weight within each family and row count.
2. Row counts receive equal weight within each family.
3. Workload families receive equal weight in the final geometric mean.

The same aggregation produces primary and end-to-end scores. A ratio of one maps to 100, while a uniform ratio of 0.1 maps to 10. The decision-grade default rejects a result when more than half of primary cases reach the 100 cap because the suite no longer provides enough optimization headroom. Exactly half remains acceptable. Quick mode reports its cap count without rejecting because its deliberately tiny scales can legitimately favor a minimal in-memory engine.

## Fairness, limitations, and anti-gaming

Amplification measures repeated work on one loaded in-memory table. It can benefit CPU caches and repeated planning paths, does not model concurrency, and retains a scale-dependent 1/160 or 1/16 of process startup and setup. The separately reported startup-inclusive score must be consulted for one-shot CLI use. Neither metric isolates only an execution kernel.

OS scheduling, filesystem cache state, CPU frequency, and other local load remain uncontrolled. Synthetic data cannot represent production compression, joins, nullability, durable storage, network access, or concurrent clients, and this benchmark makes no such claim.

Anti-gaming properties are the fixed external ClickHouse identity, configurable runtime seeds, decade scales through one million rows, deliberately conflicting data shapes, numeric and string predicates across several selectivities, three grouping cardinalities, a selective non-LIMIT projection, deterministic query ordering, alternating engine order, separate fail-closed correctness gates, identical budget-derived per-engine amplification, retained raw samples, conservative per-case caps, strict saturation rejection, and equal family/scale weighting. No single special-case query, favorable seed, or duplicated workload can legitimately stand in for the suite.
