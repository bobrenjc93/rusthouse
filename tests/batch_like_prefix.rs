use rusthouse::batch::engine::{Database, QueryResult, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{ComparisonOperator, Operand, Predicate, Statement, parse};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn predicate(sql: &str) -> Predicate {
    let statements = parse(sql).expect("valid LIKE query");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    select.predicate.clone().expect("WHERE predicate")
}

#[test]
fn parses_regular_and_distinct_prefix_like_without_the_terminal_wildcard() {
    for sql in [
        "SELECT label FROM samples WHERE label LIKE '東京%'",
        "SELECT DISTINCT label FROM samples WHERE label LIKE '東京%'",
    ] {
        assert_eq!(
            predicate(sql),
            Predicate::LikePrefix {
                column: "label".to_owned(),
                prefix: "東京".to_owned(),
            }
        );
    }

    assert_eq!(
        predicate("SELECT label FROM samples WHERE label LIKE '%'"),
        Predicate::LikePrefix {
            column: "label".to_owned(),
            prefix: String::new(),
        }
    );
    assert_eq!(
        predicate("SELECT not FROM samples WHERE not LIKE 'yes%'"),
        Predicate::LikePrefix {
            column: "not".to_owned(),
            prefix: "yes".to_owned(),
        },
        "not remains usable as a column name before LIKE",
    );
    assert_eq!(
        predicate("SELECT like FROM samples WHERE NOT like = 'a'"),
        Predicate::Not(Box::new(Predicate::Comparison {
            left: Operand::Column("like".to_owned()),
            operator: ComparisonOperator::Equal,
            right: Operand::Literal(Value::String("a".to_owned())),
        })),
        "LIKE remains usable as a column name under unary NOT",
    );
    assert_eq!(
        predicate("SELECT like FROM samples WHERE NOT like LIKE 'a%'"),
        Predicate::Not(Box::new(Predicate::LikePrefix {
            column: "like".to_owned(),
            prefix: "a".to_owned(),
        })),
        "a contextual LIKE column can itself use the LIKE operator",
    );
}

#[test]
fn parses_regular_and_distinct_contains_like_without_the_surrounding_wildcards() {
    for sql in [
        "SELECT label FROM samples WHERE label LIKE '%東京%'",
        "SELECT DISTINCT label FROM samples WHERE label LIKE '%東京%'",
    ] {
        assert_eq!(
            predicate(sql),
            Predicate::LikeContains {
                column: "label".to_owned(),
                substring: "東京".to_owned(),
            }
        );
    }

    assert_eq!(
        predicate("SELECT label FROM samples WHERE label LIKE '%%'"),
        Predicate::LikeContains {
            column: "label".to_owned(),
            substring: String::new(),
        },
    );
    assert_eq!(
        predicate("SELECT like FROM samples WHERE NOT like LIKE '%a%'"),
        Predicate::Not(Box::new(Predicate::LikeContains {
            column: "like".to_owned(),
            substring: "a".to_owned(),
        })),
        "a contextual LIKE column can use a contains predicate under unary NOT",
    );
}

#[test]
fn parses_regular_and_distinct_suffix_like_without_the_leading_wildcard() {
    for sql in [
        "SELECT label FROM samples WHERE label LIKE '%東京'",
        "SELECT DISTINCT label FROM samples WHERE label LIKE '%東京'",
    ] {
        assert_eq!(
            predicate(sql),
            Predicate::LikeSuffix {
                column: "label".to_owned(),
                suffix: "東京".to_owned(),
            }
        );
    }

    assert_eq!(
        predicate("SELECT like FROM samples WHERE NOT like LIKE '%a'"),
        Predicate::Not(Box::new(Predicate::LikeSuffix {
            column: "like".to_owned(),
            suffix: "a".to_owned(),
        })),
        "a contextual LIKE column can use a suffix predicate under unary NOT",
    );
}

#[test]
fn contextual_not_and_like_columns_remain_unambiguous() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, like String, not String); \
             INSERT INTO samples VALUES \
             (1, 'a', 'yes-one'), (2, 'b', 'no'), (3, 'b', 'yes-two');",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM samples WHERE NOT like = 'a' ORDER BY id",
        )
        .rows,
        [vec![Value::Int64(2)], vec![Value::Int64(3)]],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT like FROM samples WHERE NOT like = 'a'",
        )
        .rows,
        [vec![Value::String("b".to_owned())]],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM samples WHERE not LIKE 'yes%' ORDER BY id",
        )
        .rows,
        [vec![Value::Int64(1)], vec![Value::Int64(3)]],
    );
}

#[test]
fn executes_case_sensitive_empty_and_unicode_prefixes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, label String); \
             INSERT INTO samples VALUES \
             (1, ''), (2, 'alpha'), (3, 'alphabet'), (4, 'Alpha'), \
             (5, '東京'), (6, '東京駅'), (7, '東'), (8, 'éclair');",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM samples WHERE label LIKE 'alpha%' ORDER BY id",
        )
        .rows,
        [vec![Value::Int64(2)], vec![Value::Int64(3)]],
        "LIKE is case-sensitive",
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT label FROM samples WHERE label LIKE '東京%' ORDER BY label",
        )
        .rows,
        [
            vec![Value::String("東京".to_owned())],
            vec![Value::String("東京駅".to_owned())],
        ],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM samples WHERE label LIKE '%' ORDER BY id LIMIT 20",
        )
        .rows
        .len(),
        8,
        "the empty prefix matches every String",
    );
}

#[test]
fn executes_case_sensitive_empty_and_unicode_substrings() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, label String); \
             INSERT INTO samples VALUES \
             (1, ''), (2, 'alpha'), (3, 'xxalphayy'), (4, 'Alpha'), \
             (5, '東京'), (6, '西東京駅'), (7, '東'), (8, 'éclair');",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM samples WHERE label LIKE '%alpha%' ORDER BY id",
        )
        .rows,
        [vec![Value::Int64(2)], vec![Value::Int64(3)]],
        "contains LIKE is case-sensitive and matches away from the prefix",
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT label FROM samples WHERE label LIKE '%東京%' ORDER BY label",
        )
        .rows,
        [
            vec![Value::String("東京".to_owned())],
            vec![Value::String("西東京駅".to_owned())],
        ],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM samples WHERE label LIKE '%%' ORDER BY id LIMIT 20",
        )
        .rows
        .len(),
        8,
        "the empty substring matches every String",
    );
}

#[test]
fn executes_case_sensitive_empty_and_unicode_suffixes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, label String); \
             INSERT INTO samples VALUES \
             (1, ''), (2, 'alpha'), (3, 'xxalpha'), (4, 'Alpha'), \
             (5, '東京'), (6, '西東京'), (7, '東京駅'), (8, 'éclair');",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM samples WHERE label LIKE '%alpha' ORDER BY id",
        )
        .rows,
        [vec![Value::Int64(2)], vec![Value::Int64(3)]],
        "suffix LIKE is case-sensitive and matches away from the prefix",
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT label FROM samples WHERE label LIKE '%東京' ORDER BY label",
        )
        .rows,
        [
            vec![Value::String("東京".to_owned())],
            vec![Value::String("西東京".to_owned())],
        ],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM samples WHERE label LIKE '%' ORDER BY id LIMIT 20",
        )
        .rows
        .len(),
        8,
        "the shared empty prefix/suffix matches every String",
    );
}

#[test]
fn composes_like_with_not_and_or_in_regular_and_distinct_queries() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String, active Bool); \
             INSERT INTO events VALUES \
             (1, 'alpha', true), (2, 'alphabet', false), (3, 'beta', true), \
             (4, '東京', false), (5, '東京', true), (6, 'Alpha', true);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM events \
             WHERE NOT label LIKE '%alpha%' AND active = true OR label LIKE '%東京%' \
             ORDER BY id LIMIT 4",
        )
        .rows,
        [
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
            vec![Value::Int64(5)],
            vec![Value::Int64(6)],
        ],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT label FROM events \
             WHERE NOT (label LIKE '%alpha%' OR label LIKE '%beta%') \
             ORDER BY label",
        )
        .rows,
        [
            vec![Value::String("Alpha".to_owned())],
            vec![Value::String("東京".to_owned())],
        ],
    );
}

#[test]
fn composes_suffix_like_with_not_and_or_in_regular_and_distinct_queries() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String, active Bool); \
             INSERT INTO events VALUES \
             (1, 'alpha', true), (2, 'xxalpha', false), (3, 'beta', true), \
             (4, '東京', false), (5, '西東京', true), (6, 'Alpha', true);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM events \
             WHERE NOT label LIKE '%alpha' AND active = true OR label LIKE '%東京' \
             ORDER BY id LIMIT 4",
        )
        .rows,
        [
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
            vec![Value::Int64(5)],
            vec![Value::Int64(6)],
        ],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT label FROM events \
             WHERE NOT (label LIKE '%alpha' OR label LIKE '%beta') \
             ORDER BY label",
        )
        .rows,
        [
            vec![Value::String("Alpha".to_owned())],
            vec![Value::String("東京".to_owned())],
            vec![Value::String("西東京".to_owned())],
        ],
    );
}

#[test]
fn rejects_unsupported_and_invalid_like_patterns() {
    for sql in [
        "SELECT label FROM samples WHERE label LIKE ''",
        "SELECT label FROM samples WHERE label LIKE 'alpha'",
        "SELECT label FROM samples WHERE label LIKE 'al%pha%'",
        "SELECT label FROM samples WHERE label LIKE '%al%pha'",
        "SELECT label FROM samples WHERE label LIKE '%al%pha%'",
        "SELECT label FROM samples WHERE label LIKE 'alpha%%'",
        "SELECT label FROM samples WHERE label LIKE '%alpha%%'",
        "SELECT label FROM samples WHERE label LIKE '%%alpha%'",
        "SELECT label FROM samples WHERE label LIKE '%%alpha'",
        "SELECT label FROM samples WHERE label LIKE '%alpha%beta'",
        "SELECT label FROM samples WHERE label LIKE '%%%'",
        "SELECT label FROM samples WHERE label LIKE other",
        "SELECT label FROM samples WHERE label LIKE 1",
        "SELECT label FROM samples WHERE 'alpha' LIKE 'alpha%'",
    ] {
        assert!(
            matches!(parse(sql), Err(Error::Sql { .. })),
            "{sql:?} must return a typed SQL error",
        );
    }
}

#[test]
fn reports_non_string_like_columns_as_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, active Bool); INSERT INTO samples VALUES (1, true);",
        )
        .expect("setup");

    assert_eq!(
        database
            .execute("SELECT id FROM samples WHERE id LIKE '%1'")
            .expect_err("Int64 is not a LIKE input"),
        Error::TypeMismatch {
            context: "WHERE LIKE column 'id'".to_owned(),
            expected: DataType::String.to_string(),
            actual: DataType::Int64.to_string(),
        }
    );
    assert_eq!(
        database
            .execute("SELECT id FROM samples WHERE active LIKE '%t'")
            .expect_err("Bool is not a LIKE input"),
        Error::TypeMismatch {
            context: "WHERE LIKE column 'active'".to_owned(),
            expected: DataType::String.to_string(),
            actual: DataType::Bool.to_string(),
        }
    );
}

#[test]
fn like_atoms_retain_the_predicate_complexity_limit() {
    let atoms = std::iter::repeat_n("label LIKE '%a'", 128)
        .collect::<Vec<_>>()
        .join(" OR ");
    parse(&format!("SELECT label FROM samples WHERE NOT {atoms}"))
        .expect("128 atoms, 127 OR nodes, and one NOT exactly fit the 256-node cap");

    let too_complex = format!("SELECT label FROM samples WHERE NOT NOT {atoms}");
    assert!(matches!(
        parse(&too_complex),
        Err(Error::Sql { message, .. })
            if message == "predicate is too complex; maximum 256 expression nodes"
    ));
}

#[test]
fn like_queries_retain_scan_and_result_limits() {
    let setup = "CREATE TABLE samples (label String); \
                 INSERT INTO samples VALUES ('alpha'), ('xxalpha'), ('beta');";
    let mut scan_limited = Database::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    });
    scan_limited.execute(setup).expect("setup");
    assert_eq!(
        scan_limited
            .execute("SELECT label FROM samples WHERE label LIKE '%alpha' LIMIT 0")
            .expect_err("LIKE and LIMIT cannot bypass the full source scan"),
        Error::ResourceLimitExceeded {
            resource: "SELECT scanned rows",
            actual: 3,
            max: 2,
        }
    );

    let mut result_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        ..QueryResultLimits::default()
    });
    result_limited.execute(setup).expect("setup");
    assert_eq!(
        query(
            &mut result_limited,
            "SELECT label FROM samples WHERE label LIKE '%alpha' LIMIT 1",
        )
        .rows,
        [vec![Value::String("alpha".to_owned())]],
    );
    assert_eq!(
        result_limited
            .execute("SELECT label FROM samples WHERE label LIKE '%alpha'")
            .expect_err("both matches exceed the result row cap"),
        Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 2,
            max: 1,
        }
    );
}
