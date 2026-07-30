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

The decision-grade default uses 1,000, 10,000, and 50,000 rows, two amplified warmups, seven amplified samples, and three end-to-end samples:

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --mode default \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-default.json
~~~

The fail-closed audit runs that same suite for the documented panel `20260729`, `20260730`, and `20260731`, then publishes one aggregate details artifact:

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --mode default \
  --seeds \
  --details /tmp/rusthouse-parity-audit.json
~~~

`--seeds` is a value-free audit flag. It is mutually exclusive with `--seed`; use `--seed U64` for a single configurable exploratory run. Any setup, execution, correctness, sample-stability, or default-suite saturation failure in any panel member rejects the entire audit with score zero. A failed audit never writes a partial details artifact.

## Grouping and top-k optimization measurement

On 2026-07-29, the default command above was run on the same Apple Silicon host before and after replacing owned tree-based grouping and fully materialized sorting with borrowed hash grouping, columnar aggregate state, and index-based top-k execution. The baseline was commit `659c30b`; both runs used seed `20260729`, release binaries, the pinned ClickHouse build, and passed all 24 correctness gates. Times are RustHouse's seven-sample sustained per-query medians; the ratio is ClickHouse median divided by the optimized RustHouse median.

| Case | Rows | Before (ms) | After (ms) | Speedup | After ratio |
| --- | ---: | ---: | ---: | ---: | ---: |
| High-cardinality group by | 10,000 | 2.652 | 0.882 | 3.01x | 1.610 |
| High-cardinality group by | 50,000 | 15.124 | 4.317 | 3.50x | 0.952 |
| Numeric order by limit | 10,000 | 1.077 | 0.251 | 4.29x | 5.018 |
| Numeric order by limit | 50,000 | 6.458 | 1.096 | 5.89x | 2.935 |
| String order by limit | 10,000 | 1.769 | 0.442 | 4.00x | 2.685 |
| String order by limit | 50,000 | 11.596 | 1.985 | 5.84x | 1.279 |

The sustained score moved from 84.74 to 99.77; the startup-inclusive score was 100.00 in both runs. A second full default run with seed `20260730` passed 24/24 gates and scored 99.87. Its 50,000-row RustHouse medians were 4.271 ms for high-cardinality grouping, 1.123 ms for numeric ordering, and 1.978 ms for string ordering.

The --clickhouse flag is equivalent to RUSTHOUSE_CLICKHOUSE_BIN. The harness normally finds the prebuilt rusthouse next to itself; --rusthouse or RUSTHOUSE_BIN can override that path. A runtime --seed value deterministically changes every row count's data.

Progress is written to stderr. Stdout is exactly one compact Burner JSON object with score, summary, evidence, and suggestions. Its score is the primary sustained-work score; summary and evidence also name the end-to-end score. The --details option writes one schema-version 3 JSON document containing the seed mode and panel, aggregation hierarchy, timing method and limitations, amplification, correctness count, seed-tagged raw batch and per-query samples, medians, both ratios and scores, paths, mode, and ClickHouse identity. Each case records both its panel seed and row-count-derived dataset seed. Known option arities are pre-scanned before validation so the requested details path is cleared before reporting any parse failure, including one caused by an earlier argument, or before engine validation. After a successful audit, the complete document is written and synced to an unpredictably named sibling temporary file created exclusively without following existing symlinks, then atomically renamed into place. Setup, execution, version, checksum, parse, correctness, timing-stability, or full default-suite saturation failures still emit the one stdout object with score zero and exit nonzero without exposing stale or partial details at the requested path.

## Dataset and workloads

A dependency-free SplitMix64 generator produces deterministic typed rows. Every dataset has:

- a broad uniform integer and a 90%-near-zero skewed integer;
- a seed-shuffled permutation of unique IDs;
- eight low-cardinality string keys and unique high-cardinality keys;
- variable-length strings, including commas and SQL quotes;
- both Boolean values;
- negative numbers and signed integers around four quadrillion;
- exactly representable eighth-step floating-point values.

The first rows force important extrema, so even quick mode cannot randomly omit negative, positive, or large values. ID and high-cardinality-key assignments use separate deterministic seed-derived shuffles, preserving their exact cardinalities while changing their row layouts across seeds. Row-count-specific seed derivation prevents the larger sizes from merely timing the same prefix.

Each row count runs eight cases spanning:

| Family | Coverage |
| --- | --- |
| Full scan | COUNT, two SUMs, MIN, MAX, and AVG |
| Selective filter | A single-ID point predicate with mixed projected types |
| Compound filter | Parenthesized AND/OR, Boolean, uniform, and skewed columns |
| Nonselective filter | A predicate expected to retain about 97.5% of rows |
| Low-cardinality grouping | String plus Boolean grouping with several aggregates |
| High-cardinality grouping | Unique string-key grouping, deterministic ordering, bounded output |
| Ordering and limit | Numeric and string sort shapes with deterministic tie breakers |

The generated CREATE TABLE, INSERT, and query SQL bytes are identical for both engines. Only public output-format command-line options differ. All result-producing queries have explicit aliases and deterministic ordering where row order matters.

## Correctness gate and normalization

Correctness and timing use separate processes. Before any timing for a case, the harness runs setup plus one unamplified query on each engine, captures both outputs, and opens that case's timing gate only after normalization succeeds. Amplified and end-to-end sample acceptance both require the open gate. Any process or comparison failure rejects the entire run; failed or absent gates cannot contribute timings.

The normalizer parses standards-compliant CSV, validates exact column names and widths, and compares values using declared workload types. Integers and strings remain exact. Boolean word and numeric spellings normalize to the same value. Finite floats use a relative tolerance of 1e-9 solely for rendering and accumulation-order noise. It does not sort results, discard columns, coerce strings, or accept malformed output.

Tests cover generator reproducibility, runtime-seed variation, shuffled-key cardinality, dataset-shape and workload-diversity invariants, CSV normalization, separate correctness gating, equal engine amplification, positive amortized timings, unstable-sample rejection, per-seed score saturation detection, family/scale weighting, outer equal-seed weighting, the schema-versioned audit artifact, parse-failure cleanup, symlink-safe exclusive temporary creation, and failure-safe artifact publication.

## Timing and calibration

The primary sample starts one process, creates and inserts the dataset once, and executes the identical workload query 256 times against that in-memory table. Both engines receive exactly 256 repetitions; the runner rejects a mismatched count. Stdout goes to the null sink for timing processes. Total positive wall time is divided by 256, so startup and setup contribute only 1/256 to the reported per-query sample. Warmup process pairs are discarded, engine order alternates, and the median of measured samples is used.

The fixed 256x factor is the calibration for both quick and default modes. It was selected to make repeated analytical work the majority of ClickHouse Local batch time across the retained scales while keeping quick mode practical. A fixed shared factor avoids engine-dependent adaptive stopping and gives every case the same amortization. The harness deliberately performs no startup subtraction: subtracting independently noisy process measurements can create zero, negative, or highly unstable derived timings. Samples must remain positive, and a greater-than-10x max/min spread rejects the run.

A separate end-to-end metric times fresh processes containing setup plus one query. It uses three samples per case and includes startup, SQL parsing, table creation, insertion, execution, formatting, and process shutdown. This preserves the real CLI lifecycle signal instead of silently discarding it.

## Score aggregation

Each case ratio is ClickHouse median divided by RustHouse median. Ratios below 0.01 are floored and ratios above 1 are capped at parity before aggregation, so one unusually favorable RustHouse case cannot compensate for a slow family.

Aggregation is hierarchical in log space:

1. Workloads receive equal weight within each family and row count.
2. Row counts receive equal weight within each family.
3. Workload families receive equal weight within each seed.
4. Seeds receive equal weight in the final geometric mean, outside the existing family and scale hierarchy.

The same aggregation produces primary and end-to-end scores. A single-seed run is the one-member form of this hierarchy. A ratio of one maps to 100, while a uniform ratio of 0.1 maps to 10. The decision-grade default checks every seed independently and rejects the entire result if every primary case for any selected seed reaches the 100 cap, because that panel member measured no useful optimization headroom. Quick mode reports its cap count without rejecting because its deliberately tiny scales can legitimately favor a minimal in-memory engine.

## Fairness, limitations, and anti-gaming

Amplification measures repeated work on one loaded in-memory table. It can benefit CPU caches and repeated planning paths, does not model concurrency, and still retains 1/256 of process startup and setup. The startup-inclusive score must be consulted for one-shot CLI use. Neither metric isolates only an execution kernel.

OS scheduling, filesystem cache state, CPU frequency, and other local load remain uncontrolled. Synthetic data cannot represent production compression, joins, nullability, durable storage, network access, or concurrent clients, and this benchmark makes no such claim.

Anti-gaming properties are the fixed external ClickHouse identity, configurable single seeds, the explicit equally weighted audit panel, seed-shuffled key layouts, multiple scales, deliberately conflicting data shapes, selective and nonselective predicates, two grouping cardinalities, deterministic query ordering, alternating engine order, separate fail-closed correctness gates, identical per-engine amplification, retained seed-tagged raw samples, conservative per-case caps, and equal seed/family/scale weighting. No single special-case query, favorable seed, or duplicated workload can legitimately stand in for the suite.
