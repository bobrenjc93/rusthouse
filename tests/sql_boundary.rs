use rusthouse::{Engine, Value};

fn populated_engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .execute(
            "CREATE TABLE facts (\
                id Int64, region String, revenue Float64, paid Bool, units Nullable(Int64)\
             ) ENGINE = Memory;\
             INSERT INTO facts VALUES\
                (1, 'west', 12.5, true, 2),\
                (2, 'east', 8.0, false, NULL),\
                (3, 'west', 7.5, true, 3),\
                (4, 'east', 11.0, true, 1),\
                (5, 'north', 4.0, false, NULL);",
        )
        .unwrap();
    engine
}

#[test]
fn campaign_projection_predicate_distinct_order_and_limit_shapes() {
    let mut engine = populated_engine();
    let results = engine
        .execute(
            "SELECT id AS event_id, region, revenue + 0.5 AS adjusted \
             FROM facts \
             WHERE (paid = true AND revenue >= 7.5) OR id = 5 \
             ORDER BY region ASC, adjusted DESC LIMIT 1, 3;\
             SELECT DISTINCT region AS r FROM facts ORDER BY r DESC LIMIT 2;",
        )
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].rows,
        vec![
            vec![
                Value::Int64(5),
                Value::String("north".to_owned()),
                Value::Float64(4.5),
            ],
            vec![
                Value::Int64(1),
                Value::String("west".to_owned()),
                Value::Float64(13.0),
            ],
            vec![
                Value::Int64(3),
                Value::String("west".to_owned()),
                Value::Float64(8.0),
            ],
        ]
    );
    assert_eq!(
        results[1].rows,
        vec![
            vec![Value::String("west".to_owned())],
            vec![Value::String("north".to_owned())],
        ]
    );
}

#[test]
fn campaign_global_and_grouped_aggregate_shapes() {
    let mut engine = populated_engine();
    let results = engine
        .execute(
            "SELECT count(*) AS rows, count(units) AS known, sum(revenue) AS gross, \
                    min(units) AS smallest, max(units) AS largest, avg(units) AS mean \
             FROM facts;\
             SELECT region, paid, count(*) AS n, sum(revenue) AS total, \
                    min(revenue) AS low, max(revenue) AS high, avg(revenue) AS average \
             FROM facts GROUP BY region, paid HAVING total >= 8 \
             ORDER BY n DESC, region ASC, paid DESC LIMIT 10 OFFSET 0;",
        )
        .unwrap();

    assert_eq!(
        results[0].rows,
        vec![vec![
            Value::Int64(5),
            Value::Int64(3),
            Value::Float64(43.0),
            Value::Int64(1),
            Value::Int64(3),
            Value::Float64(2.0),
        ]]
    );
    assert_eq!(results[1].rows.len(), 3);
    assert_eq!(results[1].rows[0][0], Value::String("west".to_owned()));
    assert_eq!(results[1].rows[0][2], Value::Int64(2));
    assert_eq!(results[1].rows[0][3], Value::Float64(20.0));

    let alias_group = engine
        .execute(
            "SELECT region AS area, count(*) AS n FROM facts \
             WHERE NOT paid = false GROUP BY area ORDER BY n DESC, area",
        )
        .unwrap()
        .remove(0);
    assert_eq!(alias_group.rows[0][0], Value::String("west".to_owned()));
    assert_eq!(alias_group.rows[0][1], Value::Int64(2));
}

#[test]
fn nullable_columns_and_insert_column_lists_work_at_the_sql_boundary() {
    let mut engine = Engine::new();
    let results = engine
        .execute(
            "CREATE TABLE typed (\
                i Int64, f Nullable(Float64), b Bool, s Nullable(String)\
             );\
             INSERT INTO typed (i, b) VALUES (1, true), (2, false);\
             INSERT INTO typed VALUES (3, 2, true, 'it''s valid');\
             SELECT * FROM typed WHERE f IS NULL OR s IS NOT NULL ORDER BY i;",
        )
        .unwrap();
    assert_eq!(results[0].columns, ["i", "f", "b", "s"]);
    assert_eq!(results[0].rows.len(), 3);
    assert_eq!(results[0].rows[0][1], Value::Null);
    assert_eq!(results[0].rows[2][1], Value::Float64(2.0));
    assert_eq!(
        results[0].rows[2][3],
        Value::String("it's valid".to_owned())
    );
}

#[test]
fn malformed_and_semantically_invalid_inputs_are_rejected() {
    let malformed = [
        "SELECT 'unterminated FROM t",
        "/* unterminated",
        "CREATE TABLE t (x Decimal)",
        "CREATE TABLE t (x Int64) trailing",
        "SELECT x FROM",
        "SELECT x FROM t LIMIT 1.5",
    ];
    for sql in malformed {
        assert!(Engine::new().execute(sql).is_err(), "accepted: {sql}");
    }

    let invalid = [
        "INSERT INTO facts VALUES (6, 'west')",
        "INSERT INTO facts VALUES (6, 'west', 'bad', true, 1)",
        "SELECT missing FROM facts",
        "SELECT id FROM facts WHERE id",
        "SELECT region, sum(revenue) FROM facts",
        "SELECT sum(count(*)) FROM facts",
        "SELECT mystery(id) FROM facts",
    ];
    for sql in invalid {
        assert!(populated_engine().execute(sql).is_err(), "accepted: {sql}");
    }
}

#[test]
fn a_failed_batch_insert_is_atomic_at_the_sql_boundary() {
    let mut engine = populated_engine();
    assert!(
        engine
            .execute(
                "INSERT INTO facts VALUES \
                 (6, 'south', 2.0, true, 1), \
                 (7, 'south', 3.0, true, 'wrong')"
            )
            .is_err()
    );
    let result = engine
        .execute("SELECT count(*) AS n FROM facts")
        .unwrap()
        .remove(0);
    assert_eq!(result.rows, vec![vec![Value::Int64(5)]]);
}
