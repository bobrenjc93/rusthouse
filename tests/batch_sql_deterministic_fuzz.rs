use std::panic::{AssertUnwindSafe, catch_unwind};

use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, Statement, parse_with_limits};

const CORPUS_SEED: u64 = 0xd1ce_ba5e_5eed_f00d;
const GENERATED_CASES: usize = 512;
const MAX_GENERATED_BYTES: usize = 4_096;
const PREDICATE_DEPTH_CAP: usize = 64;
const PREDICATE_NODE_CAP: usize = 256;

const FUZZ_LIMITS: BatchSqlLimits = BatchSqlLimits {
    max_statements: 3,
    max_insert_rows: 4,
    max_insert_values: 10,
    max_ast_list_items: 8,
};

#[derive(Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, upper_bound: usize) -> usize {
        debug_assert!(upper_bound > 0);
        (self.next_u64() % upper_bound as u64) as usize
    }

    fn choose<T: Copy>(&mut self, choices: &[T]) -> T {
        choices[self.index(choices.len())]
    }
}

fn case_seed(case_index: usize) -> u64 {
    CORPUS_SEED
        .wrapping_add(case_index as u64)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

fn literal(rng: &mut SplitMix64) -> &'static str {
    rng.choose(&[
        "0",
        "-1",
        "+2",
        "3.1415",
        "6e2",
        "TRUE",
        "false",
        "''",
        "'plain'",
        "'semi;colon'",
        "'close ) open ('",
        "'雪🙂'",
        "'it''s escaped'",
    ])
}

fn generate_batch(rng: &mut SplitMix64) -> String {
    const STATEMENTS: &[&str] = &[
        "SHOW TABLES",
        "DROP TABLE old_table",
        "TRUNCATE TABLE samples",
        "EXISTS TABLE samples",
        "SELECT * FROM samples LIMIT 1",
        "INSERT INTO samples VALUES (1)",
    ];

    let statement_count = 1 + rng.index(6);
    let mut sql = ";".repeat(rng.index(3));
    for statement_index in 0..statement_count {
        if statement_index != 0 {
            sql.push_str(&";".repeat(1 + rng.index(3)));
        }
        sql.push_str(rng.choose(STATEMENTS));
    }
    sql.push_str(&";".repeat(rng.index(3)));
    sql
}

fn generate_insert(rng: &mut SplitMix64) -> String {
    let row_count = 1 + rng.index(7);
    let mut sql = String::from("INSERT INTO samples VALUES ");
    for row_index in 0..row_count {
        if row_index != 0 {
            sql.push(',');
        }
        sql.push('(');
        let value_count = 1 + rng.index(4);
        for value_index in 0..value_count {
            if value_index != 0 {
                sql.push(',');
            }
            sql.push_str(literal(rng));
        }
        sql.push(')');
    }
    sql
}

fn generate_select(rng: &mut SplitMix64) -> String {
    const ITEMS: &[&str] = &[
        "*",
        "id",
        "name AS label",
        "COUNT(*) AS rows",
        "SUM(amount) AS total",
        "LOWER(name) AS folded",
    ];
    const OPERATORS: &[&str] = &["=", "!=", "<>", "<", "<=", ">", ">="];

    let item_count = 1 + rng.index(12);
    let mut sql = String::from("SELECT ");
    for item_index in 0..item_count {
        if item_index != 0 {
            sql.push_str(", ");
        }
        sql.push_str(rng.choose(ITEMS));
    }
    sql.push_str(" FROM samples");

    if rng.index(4) != 0 {
        let term_count = 1 + rng.index(5);
        sql.push_str(" WHERE ");
        for term_index in 0..term_count {
            if term_index != 0 {
                sql.push_str(if rng.index(2) == 0 { " AND " } else { " OR " });
            }
            sql.push_str(if rng.index(2) == 0 { "id " } else { "name " });
            sql.push_str(rng.choose(OPERATORS));
            sql.push(' ');
            sql.push_str(literal(rng));
        }
    }

    if rng.index(3) == 0 {
        sql.push_str(" GROUP BY id, name");
    }
    if rng.index(3) == 0 {
        sql.push_str(" ORDER BY id ASC, name DESC");
    }
    sql
}

fn generate_nested_predicate(rng: &mut SplitMix64) -> String {
    let depth = rng.index(PREDICATE_DEPTH_CAP + 9);
    let term_count = 1 + rng.index(6);
    let mut predicate = "(".repeat(depth);
    for term_index in 0..term_count {
        if term_index != 0 {
            predicate.push_str(if rng.index(2) == 0 { " AND " } else { " OR " });
        }
        predicate.push_str("id = ");
        predicate.push_str(literal(rng));
    }
    predicate.push_str(&")".repeat(depth));
    format!("SELECT id FROM samples WHERE {predicate}")
}

fn generate_malformed(rng: &mut SplitMix64) -> String {
    const MALFORMED: &[&str] = &[
        "'unterminated",
        "SELECT FROM",
        "INSERT INTO t VALUES (1,)",
        "SELECT id FROM t WHERE (id = 1",
        "SELECT id FROM t WHERE id ! 1",
        "SELECT 1e+",
        "CREATE TABLE t (id)",
        "SHOW TABLES trailing",
        "雪 SELECT",
        "/* not a supported comment */ SELECT 1",
        "SELECT id FROM t WHERE id = '雪''",
    ];
    const TRAILING: &[&str] = &["", ";", "))", " ,", " -- 注释🙂", " \u{2003}!"];

    format!("{}{}", rng.choose(MALFORMED), rng.choose(TRAILING))
}

fn generate_token_mixture(rng: &mut SplitMix64) -> String {
    const SQL_TOKENS: &[&str] = &[
        "SELECT", "INSERT", "CREATE", "WHERE", "AND", "OR", "VALUES", "id", "Int64", "=", "!=",
        "<>", ">=",
    ];
    const UNICODE: &[&str] = &["雪", "🙂", "'λ;🙂'", "\u{2003}"];
    const DELIMITERS: &[&str] = &[",", ";", ";;", "(", ")", "((", "))"];
    const MALFORMED: &[&str] = &["!", "'open", "1e+", "@", "/*", "-- no newline"];

    // Every token-mixture case contains each feature class. Extra fragments
    // vary their order and adjacency while remaining strictly bounded.
    let mut fragments = vec![
        rng.choose(SQL_TOKENS),
        rng.choose(UNICODE),
        rng.choose(DELIMITERS),
        literal(rng),
        rng.choose(MALFORMED),
    ];
    let extra_count = rng.index(20);
    for _ in 0..extra_count {
        let fragment = match rng.index(5) {
            0 => rng.choose(SQL_TOKENS),
            1 => rng.choose(UNICODE),
            2 => rng.choose(DELIMITERS),
            3 => literal(rng),
            _ => rng.choose(MALFORMED),
        };
        fragments.push(fragment);
    }
    for index in (1..fragments.len()).rev() {
        let other = rng.index(index + 1);
        fragments.swap(index, other);
    }
    fragments.join(" ")
}

fn generate_case(case_index: usize, seed: u64) -> String {
    let mut rng = SplitMix64::new(seed);
    match case_index % 6 {
        0 => generate_batch(&mut rng),
        1 => generate_insert(&mut rng),
        2 => generate_select(&mut rng),
        3 => generate_nested_predicate(&mut rng),
        4 => generate_malformed(&mut rng),
        _ => generate_token_mixture(&mut rng),
    }
}

fn parse_generated(
    input: &str,
    limits: BatchSqlLimits,
    case_index: usize,
    seed: u64,
) -> Result<Vec<Statement>, Error> {
    catch_unwind(AssertUnwindSafe(|| parse_with_limits(input, limits))).unwrap_or_else(|payload| {
        let panic_message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        panic!(
            "parse_with_limits panicked; corpus_seed={CORPUS_SEED:#018x}; \
             case={case_index}; case_seed={seed:#018x}; input={input:?}; panic={panic_message}"
        );
    })
}

fn assert_parser_error_is_typed(error: &Error, context: &str) {
    match error {
        Error::Sql { .. }
        | Error::ReservedIdentifier { .. }
        | Error::StatementLimitExceeded { .. } => {}
        Error::ResourceLimitExceeded { resource, .. }
            if matches!(
                *resource,
                "INSERT rows" | "INSERT values" | "SQL AST list items"
            ) => {}
        error => panic!("unexpected parser error variant: {context}; error={error:?}"),
    }
}

#[test]
fn fixed_seed_generated_sql_is_bounded_panic_free_and_deterministic() {
    let mut successes = 0;
    let mut typed_failures = 0;

    for case_index in 0..GENERATED_CASES {
        let seed = case_seed(case_index);
        let input = generate_case(case_index, seed);
        let context = format!(
            "corpus_seed={CORPUS_SEED:#018x}; case={case_index}; \
             case_seed={seed:#018x}; input={input:?}"
        );
        assert!(
            input.len() <= MAX_GENERATED_BYTES,
            "generated case exceeded byte bound: {context}; bytes={}",
            input.len()
        );

        let first = parse_generated(&input, FUZZ_LIMITS, case_index, seed);
        let replay = parse_generated(&input, FUZZ_LIMITS, case_index, seed);
        assert_eq!(first, replay, "parser result changed on replay: {context}");

        match &first {
            Ok(_) => successes += 1,
            Err(error) => {
                assert_parser_error_is_typed(error, &context);
                typed_failures += 1;
            }
        }
    }

    assert!(
        successes > 0,
        "seed {CORPUS_SEED:#018x} produced no successes"
    );
    assert!(
        typed_failures > 0,
        "seed {CORPUS_SEED:#018x} produced no typed failures"
    );
}

fn nested_predicate(depth: usize) -> String {
    format!(
        "SELECT id FROM things WHERE {}id = 1{}",
        "(".repeat(depth),
        ")".repeat(depth)
    )
}

fn flat_predicate(terms: usize) -> String {
    format!(
        "SELECT id FROM things WHERE {}",
        vec!["id = 1"; terms].join(" OR ")
    )
}

#[test]
fn exact_allocation_and_predicate_caps_have_minimal_boundaries() {
    let statement_limits = BatchSqlLimits {
        max_statements: 3,
        ..BatchSqlLimits::default()
    };
    parse_with_limits(
        "SHOW TABLES; DROP TABLE t; TRUNCATE TABLE t",
        statement_limits,
    )
    .expect("exact statement cap should parse");
    assert_eq!(
        parse_with_limits(
            "SHOW TABLES; DROP TABLE t; TRUNCATE TABLE t; EXISTS TABLE t",
            statement_limits,
        ),
        Err(Error::StatementLimitExceeded {
            statements: 4,
            max_statements: 3,
        })
    );

    let exact_insert_limits = BatchSqlLimits {
        max_insert_rows: 2,
        max_insert_values: 4,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("INSERT INTO t VALUES (1, 2), (3, 4)", exact_insert_limits)
        .expect("exact INSERT row and value caps should parse");
    assert_eq!(
        parse_with_limits(
            "INSERT INTO t VALUES (1), (2), (3)",
            BatchSqlLimits {
                max_insert_rows: 2,
                max_insert_values: 3,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "INSERT rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(
        parse_with_limits(
            "INSERT INTO t VALUES (1, 2), (3, 4), (5)",
            BatchSqlLimits {
                max_insert_rows: 3,
                max_insert_values: 4,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "INSERT values",
            actual: 5,
            max: 4,
        })
    );

    let ast_sql = "SELECT a, b FROM t GROUP BY a, b ORDER BY a, b";
    parse_with_limits(
        ast_sql,
        BatchSqlLimits {
            max_ast_list_items: 6,
            ..BatchSqlLimits::default()
        },
    )
    .expect("exact AST-list cap should parse");
    assert_eq!(
        parse_with_limits(
            ast_sql,
            BatchSqlLimits {
                max_ast_list_items: 5,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 6,
            max: 5,
        })
    );

    parse_with_limits(
        &nested_predicate(PREDICATE_DEPTH_CAP),
        BatchSqlLimits::default(),
    )
    .expect("exact predicate-depth cap should parse");
    let too_deep = nested_predicate(PREDICATE_DEPTH_CAP + 1);
    assert_eq!(
        parse_with_limits(&too_deep, BatchSqlLimits::default()),
        Err(Error::Sql {
            position: "SELECT id FROM things WHERE ".len() + PREDICATE_DEPTH_CAP + 1,
            message: format!("predicate nesting exceeds limit of {PREDICATE_DEPTH_CAP}"),
        })
    );

    // A completed binary predicate always has an odd node count. 128 terms
    // use 255 nodes; the 129th comparison reaches 256, and its joining OR is
    // the first attempted node beyond the cap.
    parse_with_limits(&flat_predicate(128), BatchSqlLimits::default())
        .expect("largest complete predicate below the node cap should parse");
    let too_many_nodes = flat_predicate(129);
    assert_eq!(
        parse_with_limits(&too_many_nodes, BatchSqlLimits::default()),
        Err(Error::Sql {
            position: too_many_nodes.len(),
            message: format!(
                "predicate is too complex; maximum {PREDICATE_NODE_CAP} expression nodes"
            ),
        })
    );
}

#[test]
fn fixed_regressions_preserve_generated_edge_shapes() {
    let cases = [
        (
            "Unicode and delimiters inside nested literals",
            "SELECT note FROM t WHERE ((note = '雪;🙂(''x)'));;",
            true,
        ),
        (
            "Unicode comment before empty delimiters",
            "-- 雪;(()🙂\r\n;;;SHOW TABLES;",
            true,
        ),
        (
            "escaped quote at the end of an unterminated Unicode literal",
            "INSERT INTO t VALUES ('雪;🙂''",
            false,
        ),
        (
            "semicolon before a required closing predicate delimiter",
            "SELECT id FROM t WHERE ((id = 1);",
            false,
        ),
        ("exponent with no digits", "SELECT 1e+;", false),
        ("bare Unicode after a statement", "SHOW TABLES; 雪", false),
    ];

    for (name, input, should_succeed) in cases {
        let result = catch_unwind(AssertUnwindSafe(|| {
            parse_with_limits(input, BatchSqlLimits::default())
        }))
        .unwrap_or_else(|_| panic!("regression case panicked: {name}; input={input:?}"));

        if should_succeed {
            result.unwrap_or_else(|error| {
                panic!("regression case failed: {name}; input={input:?}; error={error:?}")
            });
        } else {
            assert!(
                matches!(result, Err(Error::Sql { .. })),
                "regression case did not return a typed SQL error: {name}; \
                 input={input:?}; result={result:?}"
            );
        }
    }
}
