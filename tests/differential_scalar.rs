use rusthouse::{Database, EvaluationContext, Value, evaluate, parse};

// Compare the SQL evaluator with independent direct computations across a
// deterministic input grid. This catches parser precedence and row binding
// errors in addition to individual operator mistakes.
#[test]
fn arithmetic_and_predicates_match_reference_computations() {
    for left in -25_i64..=25 {
        for right in -25_i64..=25 {
            let context = EvaluationContext::new()
                .with_value("left_value", left)
                .with_value("right_value", right);

            assert_eq!(
                evaluate("left_value + right_value * 3", &context).unwrap(),
                Value::Int64(left + right * 3)
            );
            assert_eq!(
                evaluate("left_value <= right_value", &context).unwrap(),
                Value::Bool(left <= right)
            );
            assert_eq!(
                evaluate(
                    "CASE WHEN left_value < right_value THEN left_value ELSE right_value END",
                    &context
                )
                .unwrap(),
                Value::Int64(left.min(right))
            );
            if right != 0 {
                assert_eq!(
                    evaluate("left_value % right_value", &context).unwrap(),
                    Value::Int64(left % right)
                );
                assert_eq!(
                    evaluate("left_value / right_value", &context).unwrap(),
                    Value::Float64(left as f64 / right as f64)
                );
            }
        }
    }
}

#[test]
fn nullable_predicate_matches_reference_filter_semantics() {
    let values = [Value::Null, (-1_i64).into(), 0_i64.into(), 1_i64.into()];
    for value in values {
        let context = EvaluationContext::new().with_value("value", value.clone());
        let actual = evaluate("value > 0 OR value IS NULL", &context).unwrap();
        let expected = Value::Bool(value.is_null() || matches!(value, Value::Int64(v) if v > 0));
        assert_eq!(actual, expected);
    }
}

#[test]
fn vectorized_select_matches_scalar_expressions_across_batches() {
    const ROWS: usize = 2053;
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE vector_input (\
             integer_value Int64 NULL, float_value Float64 NULL, \
             bool_value Bool NULL, string_value String NULL)",
        )
        .unwrap();

    let names = ["", "alpha", "beta", "delta", "omega"];
    let mut input = Vec::with_capacity(ROWS);
    for row in 0..ROWS {
        let integer = if row % 17 == 0 {
            Value::Null
        } else if row == 1 {
            Value::Int64(9_007_199_254_740_993)
        } else {
            Value::Int64((row as i64 % 101) - 50)
        };
        let float = if row % 19 == 0 {
            Value::Null
        } else if row == 1 {
            Value::Float64(9_007_199_254_740_992.0)
        } else {
            Value::Float64((row as i64 % 103) as f64 / 4.0 - 10.0)
        };
        let boolean = if row % 23 == 0 {
            Value::Null
        } else {
            Value::Bool(row % 3 == 0)
        };
        let string = if row % 29 == 0 {
            Value::Null
        } else {
            Value::String(names[row % names.len()].to_owned())
        };
        input.push(vec![integer, float, boolean, string]);
    }

    for rows in input.chunks(200) {
        let values = rows
            .iter()
            .map(|row| {
                format!(
                    "({}, {}, {}, {})",
                    sql_literal(&row[0]),
                    sql_literal(&row[1]),
                    sql_literal(&row[2]),
                    sql_literal(&row[3])
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        database
            .execute(&format!("INSERT INTO vector_input VALUES {values}"))
            .unwrap();
    }

    let operators = ["=", "!=", "<", "<=", ">", ">="];
    for operator in operators {
        for predicate in [
            format!("integer_value {operator} 7"),
            format!("integer_value {operator} 7.5"),
            format!("float_value {operator} 7.5"),
            format!("float_value {operator} 7"),
            format!("bool_value {operator} true"),
            format!("string_value {operator} 'beta'"),
        ] {
            assert_select_matches_scalar(
                &database,
                &input,
                &[3, 0],
                "string_value, integer_value",
                &predicate,
            );
        }
    }

    assert_select_matches_scalar(
        &database,
        &input,
        &[1, 3, 2],
        "float_value, string_value, bool_value",
        "integer_value > -15 AND float_value <= 20.5 AND bool_value != false AND string_value >= 'beta'",
    );
    assert_select_matches_scalar(
        &database,
        &input,
        &[0],
        "integer_value",
        "integer_value = NULL",
    );
}

fn sql_literal(value: &Value) -> String {
    match value {
        Value::Float64(value) if value.fract() == 0.0 => format!("{value}.0"),
        value => value.to_string(),
    }
}

fn assert_select_matches_scalar(
    database: &Database,
    input: &[Vec<Value>],
    projection: &[usize],
    projection_sql: &str,
    predicate: &str,
) {
    let expression = parse(predicate).unwrap();
    let expected = input
        .iter()
        .filter_map(|row| {
            let context = EvaluationContext::new()
                .with_value("integer_value", row[0].clone())
                .with_value("float_value", row[1].clone())
                .with_value("bool_value", row[2].clone())
                .with_value("string_value", row[3].clone());
            (expression.evaluate(&context).unwrap() == Value::Bool(true))
                .then(|| projection.iter().map(|index| row[*index].clone()).collect())
        })
        .collect::<Vec<Vec<Value>>>();
    let actual = database
        .execute(&format!(
            "SELECT {projection_sql} FROM vector_input WHERE {predicate}"
        ))
        .unwrap()
        .into_result_set()
        .unwrap();
    assert_eq!(actual.rows, expected, "predicate: {predicate}");
}
