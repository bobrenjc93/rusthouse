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

The comparable default uses the original 1,000, 10,000, and 50,000-row contract, two amplified warmups, seven amplified samples, three end-to-end samples, one runtime seed, and 256 repetitions at every scale:

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --mode default \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-default.json
~~~

Use audit mode for optimization decisions. It deterministically derives three SplitMix64 seeds from the supplied base seed and runs 16 cases at 50,000, 250,000, and 1,000,000 rows. It uses two amplified warmups, five amplified samples, three end-to-end samples, and fixed scale-specific repetition counts of 256, 64, and 16:

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --mode audit \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-audit.json
~~~

Audit is intentionally much more expensive than default. The three seeds, three scales, expanded query shapes, captured amplified-output checks, and repeated timing samples are all mandatory; a failed case rejects the run rather than reducing its weight.

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

The --clickhouse flag is equivalent to RUSTHOUSE_CLICKHOUSE_BIN. The harness normally finds the prebuilt rusthouse next to itself; --rusthouse or RUSTHOUSE_BIN can override that path. A runtime --seed value deterministically changes every row count's data. Quick and default use that seed directly; audit treats it as a base seed and records all three derived seeds.

Progress is written to stderr. Stdout is exactly one compact Burner JSON object with score, summary, evidence, and suggestions. Its score is the primary sustained-work score; summary and evidence also name the end-to-end score. The --details option writes schema-versioned JSON containing the timing method and limitations, scale amplification, correctness count, raw batch and per-query samples, medians, both ratios and scores, paths, base and derived seeds, per-case seed, mode, and ClickHouse identity. Setup, execution, version, checksum, parse, correctness, timing-stability, or saturation-policy failures still emit the one object with score zero and exit nonzero.

## Dataset and workloads

A dependency-free SplitMix64 generator produces deterministic typed rows. Every dataset has:

- a broad uniform integer and a 90%-near-zero skewed integer;
- eight low-cardinality string keys and unique high-cardinality keys;
- variable-length strings, including commas and SQL quotes;
- both Boolean values;
- negative numbers and signed integers around four quadrillion;
- exactly representable eighth-step floating-point values.

The first rows force important extrema, so even quick mode cannot randomly omit negative, positive, or large values. Row-count-specific seed derivation prevents the larger sizes from merely timing the same prefix. Audit first derives three seeds from the runtime base seed, then applies the same row-count derivation independently to every seed and scale.

Quick and default keep the same eight cases at each row count:

| Family | Coverage |
| --- | --- |
| Full scan | COUNT, two SUMs, MIN, MAX, and AVG |
| Selective filter | A single-ID point predicate with mixed projected types |
| Compound filter | Parenthesized AND/OR, Boolean, uniform, and skewed columns |
| Nonselective filter | A predicate expected to retain about 97.5% of rows |
| Low-cardinality grouping | String plus Boolean grouping with several aggregates |
| High-cardinality grouping | Unique string-key grouping, deterministic ordering, bounded output |
| Ordering and limit | Numeric and string sort shapes with deterministic tie breakers |

Audit retains those cases and adds eight more at every seed and scale:

| Family | Added coverage |
| --- | --- |
| Full scan | Float SUM, MIN, MAX, and AVG |
| Selectivity sweep | Numeric predicates retaining approximately 1%, 10%, and 50% |
| String predicate | String equality with integer and float aggregates |
| Intermediate-cardinality grouping | Variable payload plus Boolean grouping without LIMIT |
| Projection scan | Numeric and compound string/numeric predicates, ordered and without LIMIT |

The generated CREATE TABLE, INSERT, and query SQL bytes are identical for both engines. Only public output-format command-line options differ. All result-producing queries have explicit aliases and deterministic ordering where row order matters.

## Correctness gate and normalization

Correctness and timing use separate processes. Before any timing for a case, the harness runs setup plus one unamplified query on each engine, captures both outputs, and opens that case's single-query gate only after normalization succeeds. Audit then captures a separate batch containing two identical queries for each engine. It verifies that exactly two result sets were emitted, every cross-engine result matches, and both repetitions are stable within each engine. An audit timing gate requires both checks. Any process or comparison failure rejects the entire run; failed or absent gates cannot contribute timings.

The normalizer parses standards-compliant CSV, validates exact column names and widths, and compares values using declared workload types. Integers and strings remain exact. Boolean word and numeric spellings normalize to the same value. Finite floats use a relative tolerance of 1e-9 solely for rendering and accumulation-order noise. It does not sort results, discard columns, coerce strings, or accept malformed output.

Tests cover generator reproducibility, runtime and derived-seed variation, unchanged quick/default calibration, million-row audit configuration, dataset-shape and workload-diversity invariants, CSV and repeated-result normalization, separate correctness gating, equal engine amplification, positive amortized timings, unstable-sample rejection, score saturation detection, and seed/family/scale weighting.

## Timing and calibration

The primary sample starts one process, creates and inserts the dataset once, and executes the identical workload query repeatedly against that in-memory table. Both engines receive exactly the same fixed count for a scale; the runner rejects a mismatch. Stdout goes to the null sink for timing processes. Total positive wall time is divided by the repetition count. Warmup process pairs are discarded, engine order alternates, and the median of measured samples is used.

| Mode | Rows | Repetitions per engine |
| --- | --- | ---: |
| Quick | 256 / 2,048 | 256 / 256 |
| Default | 1,000 / 10,000 / 50,000 | 256 / 256 / 256 |
| Audit | 50,000 / 250,000 / 1,000,000 | 256 / 64 / 16 |

The fixed 256x factor remains the unchanged calibration for quick and default. Audit reduces its fixed count as scale grows so million-row batches remain executable while retaining repeated analytical work. Counts never adapt to an engine or observed timing. The harness deliberately performs no startup subtraction: subtracting independently noisy process measurements can create zero, negative, or highly unstable derived timings. Samples must remain positive, and a greater-than-10x max/min spread rejects the run.

A separate end-to-end metric times fresh processes containing setup plus one query. It uses three samples per case and includes startup, SQL parsing, table creation, insertion, execution, formatting, and process shutdown. This preserves the real CLI lifecycle signal instead of silently discarding it.

## Score aggregation

Each case ratio is ClickHouse median divided by RustHouse median. Ratios below 0.01 are floored and ratios above 1 are capped at parity before aggregation, so one unusually favorable RustHouse case cannot compensate for a slow family.

Aggregation is hierarchical in log space:

1. Workloads receive equal weight within each seed, family, and row count.
2. Row counts receive equal weight within each family.
3. Workload families receive equal weight within each seed.
4. Seeds receive equal weight in the final geometric mean.

The same aggregation produces primary and end-to-end scores. A ratio of one maps to 100, while a uniform ratio of 0.1 maps to 10. Default preserves its existing policy of rejecting only a fully capped primary suite. Audit rejects when more than 25% of primary cases reach the parity cap because broader decision-grade coverage must retain meaningful optimization headroom. Quick mode reports its cap count without rejecting because its deliberately tiny scales can legitimately favor a minimal in-memory engine.

## Fairness, limitations, and anti-gaming

Amplification measures repeated work on one loaded in-memory table. It can benefit CPU caches and repeated planning paths, does not model concurrency, and retains a scale-dependent fraction of process startup and setup. The startup-inclusive score must be consulted for one-shot CLI use. Neither metric isolates only an execution kernel.

OS scheduling, filesystem cache state, CPU frequency, and other local load remain uncontrolled. End-to-end processes include startup but are not guaranteed cold starts; the harness does not flush OS or filesystem caches. Synthetic data cannot represent production compression, joins, nullability, durable storage, network access, or concurrent clients, and this benchmark makes no such claim.

Anti-gaming properties are the fixed external ClickHouse identity, configurable runtime seed derivation, multiple scales through one million rows, deliberately conflicting data shapes, several selectivities, string/Boolean/numeric predicates, three grouping cardinalities, deterministic query ordering, alternating engine order, separate fail-closed single and amplified correctness gates, identical per-engine amplification, retained raw samples, conservative per-case caps, and equal seed/family/scale weighting. No single special-case query, favorable seed, duplicated workload, or small-scale startup advantage can legitimately stand in for the audit suite.
