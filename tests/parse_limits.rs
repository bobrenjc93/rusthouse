use rusthouse::sql::parse_with_limits;
use rusthouse::{Database, Error, ParseLimits};

fn assert_sql_limit(error: Error, expected: &str) {
    match error {
        Error::Sql { message, .. } => assert!(
            message.contains(expected),
            "expected SQL limit error containing {expected:?}, got {message:?}"
        ),
        other => panic!("expected SQL limit error containing {expected:?}, got {other}"),
    }
}

#[test]
fn sql_byte_limit_accepts_the_boundary_and_rejects_one_excess_byte() {
    let sql = "SELECT * FROM table_name";
    let mut limits = ParseLimits {
        max_sql_bytes: sql.len(),
        ..ParseLimits::default()
    };
    parse_with_limits(sql, &limits).expect("exact byte boundary succeeds");

    limits.max_sql_bytes -= 1;
    assert_sql_limit(
        parse_with_limits(sql, &limits).expect_err("excess byte fails"),
        "SQL input exceeds limit",
    );
}

#[test]
fn token_limit_accepts_the_boundary_and_rejects_the_next_token() {
    let limits = ParseLimits {
        max_tokens: 4,
        ..ParseLimits::default()
    };
    parse_with_limits("SELECT * FROM t", &limits).expect("four tokens succeed");
    assert_sql_limit(
        parse_with_limits("SELECT * FROM t;", &limits).expect_err("fifth token fails"),
        "token count exceeds limit of 4",
    );
}

#[test]
fn statement_limit_accepts_the_boundary_and_rejects_the_next_statement() {
    let limits = ParseLimits {
        max_statements: 2,
        ..ParseLimits::default()
    };
    parse_with_limits("SELECT * FROM t; SELECT * FROM t", &limits).expect("two statements succeed");
    assert_sql_limit(
        parse_with_limits("SELECT * FROM t; SELECT * FROM t; SELECT * FROM t", &limits)
            .expect_err("third statement fails"),
        "statement count exceeds limit of 2",
    );
}

#[test]
fn identifier_limit_accepts_the_boundary_and_rejects_the_next_byte() {
    let limits = ParseLimits {
        max_identifier_bytes: 1,
        ..ParseLimits::default()
    };
    parse_with_limits(
        "CREATE TABLE t (a Float64); \
         INSERT INTO t VALUES (true); \
         SELECT AVG(a) AS m FROM t WHERE a = false GROUP BY a ORDER BY m",
        &limits,
    )
    .expect("syntax and one-byte user identifiers succeed");
    assert_sql_limit(
        parse_with_limits("SELECT * FROM aa", &limits).expect_err("two-byte user identifier fails"),
        "identifier exceeds limit of 1 bytes",
    );
}

#[test]
fn literal_limit_accepts_decoded_boundary_and_rejects_the_next_byte() {
    let limits = ParseLimits {
        max_literal_bytes: 8,
        ..ParseLimits::default()
    };
    parse_with_limits("INSERT INTO t VALUES ('12345678')", &limits)
        .expect("eight-byte literal succeeds");
    assert_sql_limit(
        parse_with_limits("INSERT INTO t VALUES ('123456789')", &limits)
            .expect_err("nine-byte literal fails"),
        "literal exceeds limit of 8 bytes",
    );
    parse_with_limits("INSERT INTO t VALUES (-1234567)", &limits)
        .expect("eight-byte signed number succeeds");
    assert_sql_limit(
        parse_with_limits("INSERT INTO t VALUES (-12345678)", &limits)
            .expect_err("sign counts toward numeric literal length"),
        "literal exceeds limit of 8 bytes",
    );

    let boolean_limits = ParseLimits {
        max_literal_bytes: 4,
        ..ParseLimits::default()
    };
    parse_with_limits("INSERT INTO t VALUES (true)", &boolean_limits)
        .expect("four-byte Boolean literal succeeds");
    assert_sql_limit(
        parse_with_limits("INSERT INTO t VALUES (false)", &boolean_limits)
            .expect_err("five-byte Boolean literal fails"),
        "literal exceeds limit of 4 bytes",
    );
}

#[test]
fn schema_width_limit_accepts_the_boundary_and_rejects_the_next_column() {
    let limits = ParseLimits {
        max_schema_columns: 2,
        ..ParseLimits::default()
    };
    parse_with_limits("CREATE TABLE t (a Int64, b String)", &limits).expect("two columns succeed");
    assert_sql_limit(
        parse_with_limits("CREATE TABLE t (a Int64, b String, c Bool)", &limits)
            .expect_err("third column fails"),
        "schema exceeds limit of 2 columns",
    );
}

#[test]
fn select_item_limit_accepts_the_boundary_and_rejects_the_next_item() {
    let limits = ParseLimits {
        max_select_items: 2,
        ..ParseLimits::default()
    };
    parse_with_limits("SELECT a, b FROM t", &limits).expect("two items succeed");
    assert_sql_limit(
        parse_with_limits("SELECT a, b, c FROM t", &limits).expect_err("third item fails"),
        "SELECT list exceeds limit of 2 items",
    );
}

#[test]
fn group_by_item_limit_accepts_the_boundary_and_rejects_the_next_item() {
    let limits = ParseLimits {
        max_group_by_items: 2,
        ..ParseLimits::default()
    };
    parse_with_limits("SELECT a, b FROM t GROUP BY a, b", &limits)
        .expect("two grouping keys succeed");
    assert_sql_limit(
        parse_with_limits("SELECT a, b FROM t GROUP BY a, b, a", &limits)
            .expect_err("third grouping key fails"),
        "GROUP BY clause exceeds limit of 2 items",
    );
}

#[test]
fn group_by_limit_failure_leaves_the_entire_batch_unapplied() {
    let limits = ParseLimits {
        max_group_by_items: 1,
        ..ParseLimits::default()
    };
    let mut database = Database::with_parse_limits(limits);

    assert_sql_limit(
        database
            .execute(
                "CREATE TABLE group_not_applied (a Int64); \
                 SELECT a FROM group_not_applied GROUP BY a, a",
            )
            .expect_err("grouping limit rejects the parsed batch"),
        "GROUP BY clause exceeds limit of 1 items",
    );
    assert!(matches!(
        database.catalog().table("group_not_applied"),
        Err(Error::TableNotFound(_))
    ));
}

#[test]
fn order_by_item_limit_accepts_the_boundary_and_rejects_the_next_item() {
    let limits = ParseLimits {
        max_order_by_items: 2,
        ..ParseLimits::default()
    };
    parse_with_limits("SELECT a, b FROM t ORDER BY a, b", &limits)
        .expect("two ordering keys succeed");
    assert_sql_limit(
        parse_with_limits("SELECT a, b FROM t ORDER BY a, b, a", &limits)
            .expect_err("third ordering key fails"),
        "ORDER BY clause exceeds limit of 2 items",
    );
}

#[test]
fn order_by_limit_failure_leaves_the_entire_batch_unapplied() {
    let limits = ParseLimits {
        max_order_by_items: 1,
        ..ParseLimits::default()
    };
    let mut database = Database::with_parse_limits(limits);

    assert_sql_limit(
        database
            .execute(
                "CREATE TABLE order_not_applied (a Int64); \
                 SELECT a FROM order_not_applied ORDER BY a, a",
            )
            .expect_err("ordering limit rejects the parsed batch"),
        "ORDER BY clause exceeds limit of 1 items",
    );
    assert!(matches!(
        database.catalog().table("order_not_applied"),
        Err(Error::TableNotFound(_))
    ));
}

#[test]
fn values_cell_limit_accepts_the_boundary_and_rejects_the_next_cell() {
    let limits = ParseLimits {
        max_values_cells: 4,
        ..ParseLimits::default()
    };
    parse_with_limits("INSERT INTO t VALUES (1, 2), (3, 4)", &limits).expect("four cells succeed");
    assert_sql_limit(
        parse_with_limits("INSERT INTO t VALUES (1, 2), (3, 4, 5)", &limits)
            .expect_err("fifth cell fails"),
        "VALUES clause exceeds limit of 4 cells",
    );
}

#[test]
fn parse_limit_failure_leaves_the_entire_batch_unapplied() {
    let limits = ParseLimits {
        max_values_cells: 2,
        ..ParseLimits::default()
    };
    let mut database = Database::with_parse_limits(limits);

    assert_sql_limit(
        database
            .execute(
                "CREATE TABLE not_applied (id Int64); \
                 INSERT INTO not_applied VALUES (1), (2), (3)",
            )
            .expect_err("limit failure rejects the parsed batch"),
        "VALUES clause exceeds limit of 2 cells",
    );
    assert!(matches!(
        database.catalog().table("not_applied"),
        Err(Error::TableNotFound(_))
    ));
}

#[test]
fn database_limits_can_be_updated_between_scripts() {
    let mut database = Database::new();
    let limits = ParseLimits {
        max_select_items: 1,
        ..ParseLimits::default()
    };
    database.set_parse_limits(limits);
    assert_eq!(database.parse_limits(), &limits);
    assert_sql_limit(
        database
            .execute("SELECT a, b FROM t")
            .expect_err("updated limit applies"),
        "SELECT list exceeds limit of 1 items",
    );
}
