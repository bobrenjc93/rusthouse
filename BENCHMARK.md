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

A quick local check uses two row counts, one warmup, and three measured samples:

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --quick \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-quick.json
~~~

The decision-grade default uses 1,000, 10,000, and 50,000 rows, two warmups, and seven measured samples:

~~~bash
RUSTHOUSE_CLICKHOUSE_BIN=/path/to/clickhouse \
  target/release/clickhouse-parity-bench \
  --mode default \
  --seed 20260729 \
  --details /tmp/rusthouse-parity-default.json
~~~

The --clickhouse flag is equivalent to RUSTHOUSE_CLICKHOUSE_BIN. The harness normally finds the prebuilt rusthouse next to itself; --rusthouse or RUSTHOUSE_BIN can override that path. A runtime --seed value deterministically changes every row count's data.

Progress is written to stderr. Stdout is exactly one compact Burner JSON object with score, summary, evidence, and suggestions. The --details option adds samples, medians, ratios, paths, seed, modes, and the ClickHouse identity to a separate JSON file. Setup, execution, version, checksum, parse, or correctness failures still emit the one object with score zero and exit nonzero.

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

The generated CREATE TABLE, INSERT, and query SQL bytes are identical for both engines. Only public output-format command-line options differ. All result-producing queries have explicit aliases and deterministic ordering where row order matters.

## Correctness gate and normalization

Every warmup and measured execution is checked. Timings enter the sample vectors only after both processes succeed and their results match. Any mismatch rejects the entire run.

The normalizer parses standards-compliant CSV, validates exact column names and widths, and compares values using declared workload types. Integers and strings remain exact. Boolean word and numeric spellings normalize to the same value. Finite floats use a relative tolerance of 1e-9 solely for rendering and accumulation-order noise. It does not sort results, discard columns, coerce strings, or accept malformed output.

Tests cover generator reproducibility, runtime-seed variation, dataset-shape and workload-diversity invariants, CSV normalization, score anchor points, and the rule that a correctness failure cannot accept a timing.

## Timing and score

Each sample measures a fresh child process handling table creation, insertion, and one query. Engine order alternates to reduce systematic thermal and scheduling bias. Warmup process pairs are correctness-checked but discarded. The median of repeated wall-clock samples is reported for each workload and row count.

A case ratio is ClickHouse median divided by RustHouse median.

The final score is a robust geometric-style aggregate of these ratios. Ratios are bounded to the range 0.01 through 100 in log space, and the outer 10% is trimmed when at least ten cases exist. The result is capped at 100: parity is 100, while RustHouse taking 10 times as long maps near 10. Faster-than-ClickHouse cases can offset slower cases geometrically but cannot make the overall score exceed the parity target.

## Fairness, limitations, and anti-gaming

RustHouse currently exposes only an in-memory, run-to-completion CLI. There is no persistent server session or protocol through which an external harness can load once and time individual statements. Consequently every timing includes process startup, SQL parsing, table creation, and insertion. ClickHouse Local's substantial startup work is therefore unavoidable and visible. The same lifecycle and SQL batch are imposed on RustHouse, but these numbers are end-to-end CLI comparisons, not isolated execution-kernel measurements. A future public session interface should add a separately named query-only benchmark rather than silently changing this one.

OS scheduling, filesystem cache state, CPU frequency, and other local load remain uncontrolled. Synthetic data cannot represent production compression, joins, nullability, storage, or concurrency, and this benchmark makes no such claim.

Anti-gaming properties are the fixed external ClickHouse identity, configurable runtime seeds, multiple scales, deliberately conflicting data shapes, selective and nonselective predicates, two grouping cardinalities, deterministic query ordering, alternating engine order, per-execution correctness gates, retained raw samples, medians, and a bounded and trimmed log aggregate. No single special-case query or favorable seed can legitimately stand in for the suite.
