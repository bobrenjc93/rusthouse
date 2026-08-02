# Architecture direction

RustHouse should evolve through narrow modules with explicit boundaries:

1. A catalog owns schemas and tables.
2. A columnar storage layer owns typed vectors and validates row shape.
3. A parser produces a small typed syntax tree without coupling syntax to execution.
4. A query engine plans scans, filters, projections, grouping, aggregation, sorting, and limits.
5. Formats render results without changing execution semantics.
6. Persistence serializes catalog state atomically and rejects corrupt or incompatible data.
7. CLI and HTTP front ends share the same engine API.

The initial engine can be single-process and single-node. Public interfaces should leave room for immutable parts, parallel scans, compression, and a write-ahead log, but those are later experiments rather than premature abstractions.

Every feature should include end-to-end tests at the SQL boundary. Benchmarks should use reproducible generated data and report enough context to compare future iterations.

## Current module boundaries

- `parser` owns SQL tokenization, typed parser events, and position-aware errors.
- `evaluator` owns scalar comparison semantics.
- `database` owns catalog staging and atomic batch execution.
- `storage` owns schemas, columns, and bounded append validation.
- `csv` owns result serialization and validates complete output before writing.
- `main` is the CLI adapter over the public `Database` and CSV APIs.

The crate root is a documented API facade; implementation modules communicate
through typed values and errors instead of sharing text protocols.

## External benchmark contract

The performance evaluation runner is owned by Burner and is not part of this
repository. RustHouse only supplies the candidate binary, so output validation
and run provenance must be enforced by that external runner. A comparable run
must:

1. capture every timed repetition, including each amplified repetition;
2. hash and compare captured results after the timed interval;
3. record the RustHouse binary SHA-256 and source commit; and
4. record OS, architecture, CPU, toolchain, and performance-relevant runtime
   environment values.

Candidate-side code cannot attest to external timing or fill missing runner
metadata. Changes to those controls belong in the Burner benchmark harness.
