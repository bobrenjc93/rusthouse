use rusthouse::{Aggregate, AggregateFunction, Error, Value};

fn aggregate(function: AggregateFunction, values: &[Value]) -> Result<Value, Error> {
    let mut aggregate = Aggregate::new(function);
    for value in values {
        aggregate.add(value)?;
    }
    aggregate.finish()
}

#[test]
fn empty_and_all_null_inputs_have_sql_results() {
    for values in [&[][..], &[Value::Null, Value::Null][..]] {
        assert_eq!(
            aggregate(AggregateFunction::Count, values).unwrap(),
            0_i64.into()
        );
        for function in [
            AggregateFunction::Sum,
            AggregateFunction::Min,
            AggregateFunction::Max,
            AggregateFunction::Avg,
        ] {
            assert_eq!(aggregate(function, values).unwrap(), Value::Null);
        }
    }
    assert_eq!(
        aggregate(AggregateFunction::CountAll, &[Value::Null, Value::Null]).unwrap(),
        2_i64.into()
    );
}

#[test]
fn aggregates_ignore_null_expression_values() {
    let values = [Value::Null, 2_i64.into(), Value::Null, 4_i64.into()];
    assert_eq!(
        aggregate(AggregateFunction::Count, &values).unwrap(),
        2_i64.into()
    );
    assert_eq!(
        aggregate(AggregateFunction::CountAll, &values).unwrap(),
        4_i64.into()
    );
    assert_eq!(
        aggregate(AggregateFunction::Sum, &values).unwrap(),
        6_i64.into()
    );
    assert_eq!(
        aggregate(AggregateFunction::Min, &values).unwrap(),
        2_i64.into()
    );
    assert_eq!(
        aggregate(AggregateFunction::Max, &values).unwrap(),
        4_i64.into()
    );
    assert_eq!(
        aggregate(AggregateFunction::Avg, &values).unwrap(),
        3_f64.into()
    );
}

#[test]
fn numeric_aggregates_promote_mixed_values_and_check_integer_overflow() {
    assert_eq!(
        aggregate(
            AggregateFunction::Sum,
            &[1_i64.into(), 2.5_f64.into(), 3_i64.into()]
        )
        .unwrap(),
        6.5_f64.into()
    );
    assert!(matches!(
        aggregate(AggregateFunction::Sum, &[i64::MAX.into(), 1_i64.into()]),
        Err(Error::Overflow { .. })
    ));
    assert!(matches!(
        aggregate(AggregateFunction::Avg, &["not numeric".into()]),
        Err(Error::Aggregate(_))
    ));
}

#[test]
fn sum_promotion_and_overflow_are_independent_of_row_order() {
    let max = Value::Int64(i64::MAX);
    let one = Value::Int64(1);
    let float = Value::Float64(0.0);
    let permutations = [
        [max.clone(), one.clone(), float.clone()],
        [max.clone(), float.clone(), one.clone()],
        [one.clone(), max.clone(), float.clone()],
        [one.clone(), float.clone(), max.clone()],
        [float.clone(), max.clone(), one.clone()],
        [float, one, max],
    ];
    let expected = Value::Float64((i128::from(i64::MAX) + 1) as f64);
    for values in permutations {
        assert_eq!(
            aggregate(AggregateFunction::Sum, &values).unwrap(),
            expected
        );
    }

    let cancellable = [
        [Value::Int64(i64::MAX), Value::Int64(1), Value::Int64(-1)],
        [Value::Int64(i64::MAX), Value::Int64(-1), Value::Int64(1)],
        [Value::Int64(1), Value::Int64(i64::MAX), Value::Int64(-1)],
        [Value::Int64(1), Value::Int64(-1), Value::Int64(i64::MAX)],
        [Value::Int64(-1), Value::Int64(i64::MAX), Value::Int64(1)],
        [Value::Int64(-1), Value::Int64(1), Value::Int64(i64::MAX)],
    ];
    for values in cancellable {
        assert_eq!(
            aggregate(AggregateFunction::Sum, &values).unwrap(),
            Value::Int64(i64::MAX)
        );
    }
}

#[test]
fn min_max_compare_strings_and_reject_incompatible_domains() {
    let strings = ["pear".into(), Value::Null, "apple".into()];
    assert_eq!(
        aggregate(AggregateFunction::Min, &strings).unwrap(),
        "apple".into()
    );
    assert_eq!(
        aggregate(AggregateFunction::Max, &strings).unwrap(),
        "pear".into()
    );
    assert!(matches!(
        aggregate(AggregateFunction::Min, &[1_i64.into(), "one".into()]),
        Err(Error::Aggregate(_))
    ));
    assert!(matches!(
        aggregate(
            AggregateFunction::Max,
            &[Value::Float64(f64::NAN), "one".into()]
        ),
        Err(Error::Aggregate(_))
    ));
}

#[test]
fn nan_is_counted_and_propagates_through_numeric_aggregates() {
    let values = [1_f64.into(), Value::Float64(f64::NAN), Value::Null];
    assert_eq!(
        aggregate(AggregateFunction::Count, &values).unwrap(),
        2_i64.into()
    );
    for function in [
        AggregateFunction::Sum,
        AggregateFunction::Min,
        AggregateFunction::Max,
        AggregateFunction::Avg,
    ] {
        let Value::Float64(value) = aggregate(function, &values).unwrap() else {
            panic!("{function} should return Float64 NaN");
        };
        assert!(value.is_nan(), "{function}");
    }
}
