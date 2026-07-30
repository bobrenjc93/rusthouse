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

A quick local check uses two row counts, one query-diverse amplified warmup, three amplified samples, and three end-to-end samples:

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --quick \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-quick.json
~~~

The decision-grade default uses 1,000, 10,000, and 50,000 rows, two query-diverse amplified warmups, seven amplified samples, and three end-to-end samples:

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --mode default \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-default.json
~~~

## Grouping and top-k optimization measurement

On 2026-07-29, the default command above was run on the same Apple Silicon host before and after replacing owned tree-based grouping and fully materialized sorting with borrowed hash grouping, columnar aggregate state, and index-based top-k execution. The baseline was commit `659c30b`; both runs used seed `20260729`, release binaries, the pinned ClickHouse build, and passed all 24 correctness gates. These historical numbers used the v2 identical-query method, which v3 retains under the separately labeled `identical_query_transition` metric. They are not v3 primary-score results. Times are RustHouse's seven-sample sustained per-query medians; the ratio is ClickHouse median divided by the optimized RustHouse median.

| Case | Rows | Before (ms) | After (ms) | Speedup | After ratio |
| --- | ---: | ---: | ---: | ---: | ---: |
| High-cardinality group by | 10,000 | 2.652 | 0.882 | 3.01x | 1.610 |
| High-cardinality group by | 50,000 | 15.124 | 4.317 | 3.50x | 0.952 |
| Numeric order by limit | 10,000 | 1.077 | 0.251 | 4.29x | 5.018 |
| Numeric order by limit | 50,000 | 6.458 | 1.096 | 5.89x | 2.935 |
| String order by limit | 10,000 | 1.769 | 0.442 | 4.00x | 2.685 |
| String order by limit | 50,000 | 11.596 | 1.985 | 5.84x | 1.279 |

The v2 sustained score moved from 84.74 to 99.77; the startup-inclusive score was 100.00 in both runs. A second full v2 default run with seed `20260730` passed 24/24 gates and scored 99.87. Its 50,000-row RustHouse medians were 4.271 ms for high-cardinality grouping, 1.123 ms for numeric ordering, and 1.978 ms for string ordering.

The --clickhouse flag is equivalent to RUSTHOUSE_CLICKHOUSE_BIN. The harness normally finds the prebuilt rusthouse next to itself; --rusthouse or RUSTHOUSE_BIN can override that path. A runtime --seed value deterministically changes every row count's data and every primary query sequence.

Progress is written to stderr. Stdout is exactly one compact Burner JSON object with score, summary, evidence, and suggestions. Its score is the v3 query-diverse sustained-work score; summary and evidence also name the v2 identical-query transition score and end-to-end score. The --details option writes schema-v3 JSON containing the methodology version and limitations, amplification, correctness count, each derived sequence seed and SHA-256, all resolved variant parameters, raw batch and per-query samples, medians, ratios and scores, paths, runtime seed, mode, and ClickHouse identity. Setup, execution, version, checksum, parse, correctness, sequence-identity, timing-stability, or full default-suite saturation failures still emit the one object with score zero and exit nonzero.

## Dataset and workloads

A dependency-free SplitMix64 generator produces deterministic typed rows. Every dataset has:

- a broad uniform integer and a 90%-near-zero skewed integer;
- eight low-cardinality string keys and unique high-cardinality keys;
- variable-length strings, including commas and SQL quotes;
- both Boolean values;
- negative numbers and signed integers around four quadrillion;
- exactly representable eighth-step floating-point values.

The first rows force important extrema, so even quick mode cannot randomly omit negative, positive, or large values. Row-count-specific seed derivation prevents the larger sizes from merely timing the same prefix.

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

The generated CREATE TABLE, INSERT, and ordered query-sequence SQL bytes are identical for both engines. Only public output-format command-line options differ. All result-producing queries have explicit aliases and deterministic ordering where row order matters.

### Query-sequence bounds

Methodology `query_diverse_amplification_v3` derives an independent sequence seed from the runtime seed, row count, and stable workload name. SplitMix64 then resolves exactly 256 statements per case. Statements within a case have the same selected columns, operators, aggregate/group/order shape, and predicate structure; only documented literals change.

| Case | Resolved v3 literals and bounds |
| --- | --- |
| Full scan | Lower ID is -4,096 through -1 and upper ID is row count + 1 through row count + 4,096, so every row is retained |
| Point filter | ID is 0 through row count - 1 |
| Compound filter | Opposing Boolean literals; uniform threshold -750,000 through 250,000; skew threshold -4 through 5 |
| Nonselective filter | Uniform threshold -990,000 through -750,000, an expected 87.5% through 99.5% retention under the uniform generator |
| Low-cardinality grouping | Lower key is one of the eight generated keys; ID cutoff is 0 through 25% of row count |
| High-cardinality grouping | Lower key is between entity 0 and 50% of row count; limit is 64 through 100 |
| Numeric ordering | Score threshold is -10,000 through 0 in exact eighths; limit is 16 through 32 |
| String ordering | Payload lower key is `a`, `comma,inside`, `medium`, or `quote's payload`; limit is 16 through 32 |

The bounds deliberately span point, selective, moderately selective, and nonselective predicates while keeping each sequence within its named query family. The details file records the actual values in execution order, not only the allowed ranges.

## Correctness gate and normalization

Correctness and timing use separate processes. Before primary timing for a case, the harness sends the complete 256-statement sequence to each engine, captures every result, and opens the timing gate only after all 256 normalized results match. The gate records the query count and SHA-256 and accepts timing only when both engines report that exact digest and count. The transition and end-to-end methods have separate full-output gates for their exact sequences. Any process, metadata, or comparison failure rejects the entire run; failed or absent gates cannot contribute timings.

The normalizer parses standards-compliant CSV, validates exact column names and widths, and compares values using declared workload types. Integers and strings remain exact. Boolean word and numeric spellings normalize to the same value. Finite floats use a relative tolerance of 1e-9 solely for rendering and accumulation-order noise. It does not sort results, discard columns, coerce strings, or accept malformed output.

Tests cover generator reproducibility, runtime-seed variation, parameter bounds and within-batch diversity, SHA-256 vectors, byte-preserving sequence assembly, multi-result CSV normalization, digest-bound correctness gating, equal engine amplification, positive amortized timings, unstable-sample rejection, score saturation detection, and family/scale weighting.

## Timing and calibration

The v3 primary sample starts one process, creates and inserts the dataset once, and executes the 256 seed-derived variants against that in-memory table. Both engines receive the same sequence bytes and exactly 256 statements; the runner rejects a mismatched count or digest. Stdout goes to the null sink only for timing processes. Total positive wall time is divided by 256, so startup and setup contribute only 1/256 to the reported per-query sample. Warmup process pairs are discarded, engine order alternates, and the median of measured samples is used.

The fixed 256x factor, warmup counts, measured sample counts, stability threshold, and medians are unchanged from v2 for both quick and default modes. A fixed shared factor avoids engine-dependent adaptive stopping and gives every case the same amortization. The harness deliberately performs no startup subtraction: subtracting independently noisy process measurements can create zero, negative, or highly unstable derived timings. Samples must remain positive, and a greater-than-10x max/min spread rejects the run.

For transition analysis, every case also runs the former v2 batch of 256 byte-identical copies of the original query with the same warmups and samples. Its hierarchically aggregated result is labeled `identical_query_transition_score`; it never supplies the Burner `score` and cannot replace the v3 correctness or saturation gate.

A separate end-to-end metric times fresh processes containing setup plus one query. It uses three samples per case and includes startup, SQL parsing, table creation, insertion, execution, formatting, and process shutdown. This preserves the real CLI lifecycle signal instead of silently discarding it.

## Score aggregation

Each case ratio is ClickHouse median divided by RustHouse median. Ratios below 0.01 are floored and ratios above 1 are capped at parity before aggregation, so one unusually favorable RustHouse case cannot compensate for a slow family.

Aggregation is hierarchical in log space:

1. Workloads receive equal weight within each family and row count.
2. Row counts receive equal weight within each family.
3. Workload families receive equal weight in the final geometric mean.

The same aggregation and caps produce primary, identical-query transition, and end-to-end scores. A ratio of one maps to 100, while a uniform ratio of 0.1 maps to 10. The decision-grade default rejects a result if every v3 primary case reaches the 100 cap because that indicates no useful optimization headroom was measured. Quick mode reports its cap count without rejecting because its deliberately tiny scales can legitimately favor a minimal in-memory engine.

## Fairness, limitations, and anti-gaming

Amplification measures varied work on one loaded in-memory table. It can still benefit CPU and data caches, does not model cold caches or concurrency, and retains 1/256 of process startup and setup. The startup-inclusive score must be consulted for one-shot CLI use. The explicitly labeled identical-query transition metric remains more exposed to repeated-plan and answer-cache artifacts. None of the metrics isolates only an execution kernel.

OS scheduling, filesystem cache state, CPU frequency, and other local load remain uncontrolled. Synthetic data cannot represent production compression, joins, nullability, durable storage, network access, or concurrent clients, and this benchmark makes no such claim.

Anti-gaming properties are the fixed external ClickHouse identity, configurable runtime seeds, multiple scales, deliberately conflicting data shapes, bounded within-batch literal/key/selectivity variation, two grouping cardinalities, deterministic query ordering, recorded resolved parameters and sequence digests, alternating engine order, complete amplified-output fail-closed gates, byte-identical symmetric amplification, retained raw samples, conservative per-case caps, and unchanged equal family/scale weighting. No single special-case query, favorable seed, repeated-answer cache, or duplicated workload can legitimately stand in for the suite.
