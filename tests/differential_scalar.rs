use rusthouse::{EvaluationContext, Value, evaluate};

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
