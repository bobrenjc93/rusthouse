//! Deterministic execution-budget and spill boundary tests.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use rusthouse::format::{OutputFormat, render, render_with_limit};
use rusthouse::{
    DataType, Database, Error, ExecutionLimits, QueryResult, Resource, ResultColumn,
    StatementResult, Value,
};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .pop()
        .expect("statement result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn assert_limit(error: Error, resource: Resource, limit: usize, actual: usize) {
    assert_eq!(
        error,
        Error::ResourceLimitExceeded {
            resource,
            limit,
            actual,
        }
    );
}

#[test]
fn input_token_and_statement_limits_fail_at_the_first_excess() {
    let sql = "SELECT * FROM t";
    let mut database = Database::with_limits(ExecutionLimits {
        max_input_bytes: sql.len() - 1,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute(sql)
            .expect_err("input is one byte too long"),
        Resource::InputBytes,
        sql.len() - 1,
        sql.len(),
    );
    assert_eq!(database.last_execution_stats().input_bytes, sql.len());
    assert_eq!(database.last_execution_stats().tokens, 0);
    assert_eq!(database.last_execution_stats().statements, 0);

    database.set_limits(ExecutionLimits {
        max_tokens: 3,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database.execute(sql).expect_err("fourth token is rejected"),
        Resource::Tokens,
        3,
        4,
    );
    assert_eq!(database.last_execution_stats().tokens, 4);
    assert_eq!(database.last_execution_stats().statements, 0);

    database.set_limits(ExecutionLimits {
        max_statements: 1,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute("SELECT * FROM t; SELECT * FROM t")
            .expect_err("second statement is rejected before execution"),
        Resource::Statements,
        1,
        2,
    );
    assert_eq!(database.last_execution_stats().tokens, 9);
    assert_eq!(database.last_execution_stats().statements, 2);
    assert!(database.catalog().table("t").is_err());

    database.set_limits(ExecutionLimits::default());
    assert!(matches!(
        database.execute("SELECT FROM"),
        Err(Error::Sql { .. })
    ));
    assert_eq!(database.last_execution_stats().tokens, 2);
    assert_eq!(database.last_execution_stats().statements, 0);
}

#[test]
fn schema_and_stored_value_limits_leave_catalog_mutations_atomic() {
    let mut database = Database::with_limits(ExecutionLimits {
        max_schema_width: 2,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute("CREATE TABLE wide (a Int64, b Int64, c Int64)")
            .expect_err("third column is rejected"),
        Resource::SchemaWidth,
        2,
        3,
    );
    assert_eq!(database.last_execution_stats().schema_width, 3);
    assert_eq!(database.last_execution_stats().statements, 0);
    assert!(database.catalog().table("wide").is_err());

    database.set_limits(ExecutionLimits {
        max_stored_values: 4,
        ..ExecutionLimits::default()
    });
    database
        .execute("CREATE TABLE narrow (a Int64, b String)")
        .expect("schema fits");
    database
        .execute("INSERT INTO narrow VALUES (1, 'a'), (2, 'b')")
        .expect("four values fit exactly");
    assert_limit(
        database
            .execute("INSERT INTO narrow VALUES (3, 'c')")
            .expect_err("fifth and sixth values are rejected"),
        Resource::StoredValues,
        4,
        6,
    );
    assert_eq!(database.catalog().table("narrow").unwrap().row_count(), 2);
    assert_eq!(database.last_execution_stats().stored_values, 4);
}

#[test]
fn intermediate_and_result_rows_have_independent_exact_limits() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE t (n Int64); INSERT INTO t VALUES (3), (1), (2)")
        .expect("setup succeeds");

    database.set_limits(ExecutionLimits {
        max_intermediate_rows: 2,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute("SELECT n FROM t ORDER BY n")
            .expect_err("third sorter input is rejected"),
        Resource::IntermediateRows,
        2,
        3,
    );

    database.set_limits(ExecutionLimits {
        max_result_rows: 2,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute("SELECT n FROM t")
            .expect_err("third result is rejected"),
        Resource::ResultRows,
        2,
        3,
    );
    assert_eq!(database.last_execution_stats().result_rows, 2);
}

#[test]
fn retained_results_reduce_sort_memory_across_statements() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE t (n Int64);
             INSERT INTO t VALUES (9), (8), (7), (6), (5), (4), (3), (2), (1)",
        )
        .expect("setup succeeds");
    database.set_limits(ExecutionLimits {
        max_memory_bytes: 144,
        ..ExecutionLimits::default()
    });

    let error = database
        .execute("SELECT n FROM t LIMIT 1; SELECT n FROM t ORDER BY n LIMIT 1")
        .expect_err("the retained first result leaves too little batch memory");
    assert!(matches!(
        error,
        Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit: 144,
            actual
        } if actual > 144
    ));
    assert_eq!(database.last_execution_stats().result_rows, 1);
    assert!(database.last_execution_stats().peak_memory_bytes <= 144);
}

#[test]
fn sorter_success_is_monotonic_across_capacity_boundaries() {
    let values = (1..=17)
        .rev()
        .map(|value| format!("({value})"))
        .collect::<Vec<_>>()
        .join(",");
    let mut database = Database::new();
    database
        .execute(&format!(
            "CREATE TABLE t (n Int64); INSERT INTO t VALUES {values}"
        ))
        .expect("setup succeeds");

    for memory in [352, 368, 384] {
        database.set_limits(ExecutionLimits {
            max_memory_bytes: memory,
            ..ExecutionLimits::default()
        });
        let result = query(&mut database, "SELECT n FROM t ORDER BY n LIMIT 1");
        assert_eq!(result.rows, vec![vec![Value::Int64(1)]]);
        assert!(database.last_execution_stats().peak_memory_bytes <= memory);
    }
}

#[test]
fn empty_and_wide_result_metadata_is_memory_accounted() {
    let definitions = (0..64)
        .map(|column| format!("column_{column} Int64"))
        .collect::<Vec<_>>()
        .join(",");
    let mut database = Database::new();
    database
        .execute(&format!("CREATE TABLE wide ({definitions})"))
        .expect("setup succeeds");

    database.set_limits(ExecutionLimits {
        max_memory_bytes: 0,
        ..ExecutionLimits::default()
    });
    assert!(matches!(
        database.execute("SELECT * FROM wide LIMIT 0"),
        Err(Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit: 0,
            actual
        }) if actual > 0
    ));

    database.set_limits(ExecutionLimits::default());
    query(&mut database, "SELECT * FROM wide LIMIT 0");
    let one_result_memory = database.last_execution_stats().peak_memory_bytes;
    assert!(one_result_memory > 0);

    database.set_limits(ExecutionLimits {
        max_memory_bytes: one_result_memory,
        ..ExecutionLimits::default()
    });
    let error = database
        .execute("SELECT * FROM wide LIMIT 0; SELECT * FROM wide LIMIT 0")
        .expect_err("second result metadata must consume additional memory");
    assert!(matches!(
        error,
        Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit,
            actual
        } if limit == one_result_memory && actual > limit
    ));
    assert!(database.last_execution_stats().peak_memory_bytes <= one_result_memory);
}

#[test]
fn amplified_projection_and_ordering_plans_are_bounded_before_allocation() {
    let definitions = (0..128)
        .map(|column| format!("c{column} Int64"))
        .collect::<Vec<_>>()
        .join(",");
    let mut database = Database::new();
    database
        .execute(&format!(
            "CREATE TABLE ordered (n Int64); CREATE TABLE wide ({definitions})"
        ))
        .expect("setup succeeds");

    query(&mut database, "SELECT n FROM ordered LIMIT 0");
    let baseline_memory = database.last_execution_stats().peak_memory_bytes;
    let ordering_limit = baseline_memory.saturating_add(128);
    database.set_limits(ExecutionLimits {
        max_memory_bytes: ordering_limit,
        ..ExecutionLimits::default()
    });
    let ordering = std::iter::repeat_n("n", 100_000)
        .collect::<Vec<_>>()
        .join(",");
    assert!(matches!(
        database.execute(&format!(
            "SELECT n FROM ordered ORDER BY {ordering} LIMIT 0"
        )),
        Err(Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit,
            actual
        }) if limit == ordering_limit && actual > limit
    ));
    assert!(database.last_execution_stats().peak_memory_bytes <= ordering_limit);

    database.set_limits(ExecutionLimits {
        max_memory_bytes: 4_096,
        ..ExecutionLimits::default()
    });
    let projections = std::iter::repeat_n("*", 1_000)
        .collect::<Vec<_>>()
        .join(",");
    assert!(matches!(
        database.execute(&format!(
            "SELECT {projections} FROM wide LIMIT 0"
        )),
        Err(Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit: 4_096,
            actual
        }) if actual > 4_096
    ));
    assert!(database.last_execution_stats().peak_memory_bytes <= 4_096);
}

#[test]
fn grouped_string_values_are_checked_before_clone_and_group_growth() {
    let large_key = "!".repeat(200_000);
    let mut rows = vec![format!("('{large_key}', 0)")];
    rows.extend((1..=16).map(|value| format!("('g{value}', {value})")));
    let mut database = Database::new();
    database
        .execute(&format!(
            "CREATE TABLE grouped_values (label String, n Int64);
             INSERT INTO grouped_values VALUES {}",
            rows.join(",")
        ))
        .expect("setup succeeds");
    database.set_limits(ExecutionLimits {
        max_memory_bytes: 512,
        ..ExecutionLimits::default()
    });

    assert!(matches!(
        database.execute(
            "SELECT label, COUNT(*) FROM grouped_values GROUP BY label LIMIT 0"
        ),
        Err(Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit: 512,
            actual
        }) if actual > 512
    ));
    assert!(database.last_execution_stats().peak_memory_bytes <= 512);

    assert!(matches!(
        database.execute(
            "SELECT label, COUNT(*) FROM grouped_values
             WHERE label >= 'g' GROUP BY label LIMIT 0"
        ),
        Err(Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit: 512,
            actual
        }) if actual > 512
    ));
    assert!(database.last_execution_stats().peak_memory_bytes <= 512);
}

#[test]
fn predicate_literals_are_borrowed_and_compiled_nodes_are_bounded() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE predicate_values (label String)")
        .expect("setup succeeds");
    query(&mut database, "SELECT label FROM predicate_values LIMIT 0");
    let baseline_memory = database.last_execution_stats().peak_memory_bytes;
    let memory_limit = baseline_memory.saturating_add(64);
    database.set_limits(ExecutionLimits {
        max_memory_bytes: memory_limit,
        ..ExecutionLimits::default()
    });

    let large_literal = "x".repeat(2 * 1024 * 1024);
    let result = query(
        &mut database,
        &format!(
            "SELECT label FROM predicate_values
             WHERE label = '{large_literal}' LIMIT 0"
        ),
    );
    assert!(result.rows.is_empty());
    assert!(database.last_execution_stats().peak_memory_bytes <= memory_limit);

    let compound = std::iter::repeat_n("label = 'x'", 100)
        .collect::<Vec<_>>()
        .join(" OR ");
    assert!(matches!(
        database.execute(&format!(
            "SELECT label FROM predicate_values WHERE {compound} LIMIT 0"
        )),
        Err(Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit,
            actual
        }) if limit == memory_limit && actual > limit
    ));
    assert!(database.last_execution_stats().peak_memory_bytes <= memory_limit);
}

#[test]
fn sorting_and_grouping_spill_with_deterministic_results_and_cleanup() {
    let rows = (0..120)
        .map(|number| format!("({}, {}, '{}')", number, number % 3, 120 - number))
        .collect::<Vec<_>>()
        .join(",");
    let spill_directory = std::env::temp_dir().join(format!(
        "rusthouse-spill-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    fs::create_dir(&spill_directory).expect("create isolated spill directory");
    let mut database = Database::with_limits_and_spill_directory(
        ExecutionLimits {
            max_memory_bytes: 768,
            ..ExecutionLimits::default()
        },
        &spill_directory,
    );
    database
        .execute(&format!(
            "CREATE TABLE t (n Int64, bucket Int64, label String); INSERT INTO t VALUES {rows}"
        ))
        .expect("setup succeeds");

    let ordered = query(
        &mut database,
        "SELECT n, label FROM t ORDER BY label, n DESC LIMIT 4",
    );
    assert_eq!(
        ordered.rows,
        vec![
            vec![Value::Int64(119), Value::String("1".to_owned())],
            vec![Value::Int64(110), Value::String("10".to_owned())],
            vec![Value::Int64(20), Value::String("100".to_owned())],
            vec![Value::Int64(19), Value::String("101".to_owned())],
        ]
    );
    assert!(database.last_execution_stats().spill_runs > 0);
    assert!(database.last_execution_stats().spilled_bytes > 0);
    assert_eq!(database.last_execution_stats().statements, 1);
    assert_eq!(database.last_execution_stats().stored_values, 360);
    assert_eq!(database.last_execution_stats().intermediate_rows, 120);
    assert_eq!(database.last_execution_stats().result_rows, 4);
    assert!(database.last_execution_stats().peak_memory_bytes <= 768);

    database.set_limits(ExecutionLimits {
        max_memory_bytes: 120,
        ..ExecutionLimits::default()
    });
    let error = database
        .execute("SELECT n FROM t ORDER BY n")
        .expect_err("two merge heads do not fit with retained metadata");
    assert!(matches!(
        error,
        Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit: 120,
            actual
        } if actual > 120
    ));
    assert_eq!(
        fs::read_dir(&spill_directory)
            .expect("read spill directory after error")
            .count(),
        0,
        "temporary runs are removed after an error"
    );

    database.set_limits(ExecutionLimits {
        max_memory_bytes: 1_280,
        ..ExecutionLimits::default()
    });

    let grouped = query(
        &mut database,
        "SELECT bucket, COUNT(*) AS rows, SUM(n) AS total
         FROM t GROUP BY bucket ORDER BY total DESC LIMIT 2",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![Value::Int64(2), Value::Int64(40), Value::Int64(2420)],
            vec![Value::Int64(1), Value::Int64(40), Value::Int64(2380)],
        ]
    );
    assert!(database.last_execution_stats().spill_runs > 0);
    assert_eq!(database.last_execution_stats().intermediate_rows, 123);
    assert_eq!(database.last_execution_stats().result_rows, 2);
    assert!(database.last_execution_stats().peak_memory_bytes <= 1_280);
    assert_eq!(
        fs::read_dir(&spill_directory)
            .expect("read spill directory")
            .count(),
        0,
        "temporary runs are removed after each query"
    );
    fs::remove_dir(&spill_directory).expect("remove isolated spill directory");
}

#[test]
fn many_tiny_runs_keep_spill_metadata_and_live_files_bounded() {
    let rows = (0..512)
        .rev()
        .map(|number| format!("({number})"))
        .collect::<Vec<_>>()
        .join(",");
    let spill_directory = std::env::temp_dir().join(format!(
        "rusthouse-many-runs-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    fs::create_dir(&spill_directory).expect("create isolated spill directory");
    let mut database = Database::with_limits_and_spill_directory(
        ExecutionLimits {
            max_memory_bytes: 384,
            ..ExecutionLimits::default()
        },
        &spill_directory,
    );
    database
        .execute(&format!(
            "CREATE TABLE many (n Int64); INSERT INTO many VALUES {rows}"
        ))
        .expect("setup succeeds");

    let result = query(&mut database, "SELECT n FROM many ORDER BY n LIMIT 1");
    assert_eq!(result.rows, vec![vec![Value::Int64(0)]]);
    assert!(database.last_execution_stats().spill_runs >= 40);
    assert!(database.last_execution_stats().peak_live_spill_runs <= 4);
    assert!(database.last_execution_stats().peak_memory_bytes <= 384);
    assert_eq!(
        fs::read_dir(&spill_directory)
            .expect("read spill directory")
            .count(),
        0
    );
    fs::remove_dir(&spill_directory).expect("remove isolated spill directory");
}

#[test]
fn string_extrema_do_not_clone_discarded_large_candidates() {
    let large_min_candidate = "z".repeat(200_000);
    let large_max_candidate = "a".repeat(200_000);
    let mut database = Database::new();
    database
        .execute(&format!(
            "CREATE TABLE extrema (min_text String, max_text String);
             INSERT INTO extrema VALUES
                ('{large_min_candidate}', '{large_max_candidate}'),
                ('a', 'z')"
        ))
        .expect("setup succeeds");
    database.set_limits(ExecutionLimits {
        max_memory_bytes: 512,
        ..ExecutionLimits::default()
    });

    let result = query(
        &mut database,
        "SELECT MIN(min_text) AS low, MAX(max_text) AS high FROM extrema",
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::String("a".to_owned()),
            Value::String("z".to_owned()),
        ]]
    );
    assert!(database.last_execution_stats().peak_memory_bytes <= 512);

    assert!(matches!(
        database.execute("SELECT MAX(min_text) FROM extrema"),
        Err(Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit: 512,
            actual
        }) if actual > 512
    ));
}

#[test]
fn memory_and_rendered_byte_limits_report_deterministic_sizes() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE t (label String); INSERT INTO t VALUES ('abcdef')")
        .expect("setup succeeds");
    database.set_limits(ExecutionLimits {
        max_memory_bytes: 8,
        ..ExecutionLimits::default()
    });
    let error = database
        .execute("SELECT label FROM t")
        .expect_err("owned result cannot fit");
    assert!(matches!(
        error,
        Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit: 8,
            actual
        } if actual > 8
    ));

    database.set_limits(ExecutionLimits::default());
    let result = query(&mut database, "SELECT label FROM t");
    let complete = render(&result, OutputFormat::Json);
    assert_eq!(
        render_with_limit(&result, OutputFormat::Json, complete.len())
            .expect("exact rendered size is accepted"),
        complete
    );
    assert_limit(
        render_with_limit(&result, OutputFormat::Json, complete.len() - 1)
            .expect_err("one byte below exact size is rejected"),
        Resource::RenderedBytes,
        complete.len() - 1,
        complete.len(),
    );
}

#[test]
fn table_and_csv_render_limits_count_streamed_escaping_exactly() {
    let result = QueryResult {
        columns: vec![ResultColumn {
            name: "payload".to_owned(),
            data_type: DataType::String,
        }],
        rows: (0..256)
            .map(|_| vec![Value::String("\u{0000}\u{0007},\"quoted\"\n".repeat(32))])
            .collect(),
    };

    for format in [OutputFormat::Table, OutputFormat::Csv] {
        let complete = render(&result, format);
        assert_limit(
            render_with_limit(&result, format, 0).expect_err("zero bytes rejects output"),
            Resource::RenderedBytes,
            0,
            complete.len(),
        );
        assert_eq!(
            render_with_limit(&result, format, complete.len())
                .expect("exact streamed size is accepted"),
            complete
        );
    }
}
