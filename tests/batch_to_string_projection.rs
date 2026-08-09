use std::mem::size_of;

use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, SelectItem, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected exactly one query result");
    };
    result.clone()
}

#[test]
fn parses_case_insensitive_to_string_with_alias_and_expression_ordering() {
    let statements = parse(
        "SELECT toString(value), TOSTRING(value) AS rendered FROM samples \
         ORDER BY ToStRiNg(value) DESC LIMIT 2 OFFSET 1",
    )
    .expect("valid toString projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::ToString {
                name: "value".to_owned(),
                alias: None,
            },
            SelectItem::ToString {
                name: "value".to_owned(),
                alias: Some("rendered".to_owned()),
            },
        ]
    );
    assert_eq!(select.order_by[0].name, "toString(value)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));
    assert_eq!(select.offset, Some(1));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT toString(value) FROM samples", limits)
        .expect("one toString item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT toString(value), value FROM samples", limits,),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn matches_cast_to_string_for_numeric_and_bool_columns_and_is_identity_for_string() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES \
             (-9223372036854775808, -0.0, false, ''), \
             (0, 5e-324, true, '東京'), \
             (9223372036854775807, 1.7976931348623157e308, false, 'quote''d');",
        )
        .expect("setup");

    let result = query(
        &mut database,
        "SELECT toString(i), CAST(i AS String), \
                toString(f), CAST(f AS String), \
                toString(b), CAST(b AS String), \
                toString(s), s \
         FROM samples",
    );
    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "toString(i)".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "CAST(i AS String)".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "toString(f)".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "CAST(f AS String)".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "toString(b)".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "CAST(b AS String)".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "toString(s)".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "s".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    for row in &result.rows {
        assert_eq!(row[0], row[1]);
        assert_eq!(row[2], row[3]);
        assert_eq!(row[4], row[5]);
        assert_eq!(row[6], row[7]);
    }
    assert_eq!(result.rows[0][2], Value::String("-0".to_owned()));
    assert_eq!(result.rows[1][6], Value::String("東京".to_owned()));
    assert_eq!(result.rows[2][6], Value::String("quote'd".to_owned()));
}

#[test]
fn preserves_filtering_aliases_expression_ordering_and_pagination() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, value Int64, keep Bool); \
             INSERT INTO samples VALUES \
             (1, 2, true), (2, 10, true), (3, -1, false), \
             (4, -10, true), (5, 1, true), (6, 20, true);",
        )
        .expect("setup");

    let result = query(
        &mut database,
        "SELECT id, toString(value) AS rendered FROM samples \
         WHERE keep = true ORDER BY TOSTRING(value) LIMIT 3 OFFSET 1",
    );
    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "rendered".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![Value::Int64(5), Value::String("1".to_owned())],
            vec![Value::Int64(2), Value::String("10".to_owned())],
            vec![Value::Int64(1), Value::String("2".to_owned())],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT toString(value) AS rendered FROM samples \
             WHERE keep = true ORDER BY rendered DESC LIMIT 2",
        )
        .rows,
        [
            vec![Value::String("20".to_owned())],
            vec![Value::String("2".to_owned())],
        ]
    );
}

#[test]
fn rejects_missing_grouped_and_malformed_to_string_shapes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT toString(missing) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );
    for sql in [
        "SELECT toString(s) FROM samples GROUP BY s",
        "SELECT toString(i), COUNT(*) FROM samples",
        "SELECT toString(i), COUNT(*) FROM samples GROUP BY i",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::InvalidQuery(
                "toString projections are only supported in ungrouped SELECT queries".to_owned()
            )),
            "{sql}"
        );
    }

    for sql in [
        "SELECT toString() FROM samples",
        "SELECT toString(*) FROM samples",
        "SELECT toString('one') FROM samples",
        "SELECT toString(i, f) FROM samples",
        "SELECT toString(i FROM samples",
        "SELECT toString(i) rendered FROM samples",
        "SELECT toString(toString(i)) FROM samples",
        "SELECT toString(i AS String) FROM samples",
        "SELECT toString(i) FROM samples ORDER BY toString()",
        "SELECT toString(i) FROM samples ORDER BY toString(*)",
        "SELECT toString(i) FROM samples ORDER BY toString(i",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn obeys_row_value_and_generated_string_byte_result_bounds() {
    let mut shape_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 3,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    shape_limited
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .expect("setup");
    assert_eq!(
        shape_limited.execute("SELECT toString(value) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(
        shape_limited.execute("SELECT toString(value), toString(value) FROM samples LIMIT 2",),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 4,
            max: 3,
        })
    );

    let result_name = "rendered";
    let fixed_bytes = size_of::<ResultColumn>()
        + result_name.len()
        + size_of::<Vec<Value>>()
        + size_of::<Value>();
    let exact_bytes = fixed_bytes + "-9223372036854775808".len();
    let setup = "CREATE TABLE samples (value Int64); \
                 INSERT INTO samples VALUES (-9223372036854775808);";
    let sql = "SELECT toString(value) AS rendered FROM samples";

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(setup).expect("setup");
    assert_eq!(
        query(&mut exact, sql).rows,
        [vec![Value::String("-9223372036854775808".to_owned())]]
    );

    let mut one_byte_short = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: exact_bytes - 1,
        ..QueryResultLimits::default()
    });
    one_byte_short.execute(setup).expect("setup");
    assert_eq!(
        one_byte_short.execute(sql),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            actual: exact_bytes,
            max: exact_bytes - 1,
        })
    );
}
