use rusthouse::{
    DataType, Error, EvaluationContext, Expr, MAX_EXPRESSION_DEPTH, UnaryOperator, Value, evaluate,
    parse,
};

fn eval(sql: &str) -> Value {
    evaluate(sql, &EvaluationContext::new()).unwrap()
}

#[test]
fn and_uses_sql_three_valued_truth_table() {
    let values = ["FALSE", "TRUE", "NULL"];
    let expected = [
        [Value::Bool(false), Value::Bool(false), Value::Bool(false)],
        [Value::Bool(false), Value::Bool(true), Value::Null],
        [Value::Bool(false), Value::Null, Value::Null],
    ];

    for (left_index, left) in values.iter().enumerate() {
        for (right_index, right) in values.iter().enumerate() {
            assert_eq!(
                eval(&format!("{left} AND {right}")),
                expected[left_index][right_index],
                "{left} AND {right}"
            );
        }
    }
}

#[test]
fn or_and_not_use_sql_three_valued_truth_tables() {
    let values = ["FALSE", "TRUE", "NULL"];
    let expected_or = [
        [Value::Bool(false), Value::Bool(true), Value::Null],
        [Value::Bool(true), Value::Bool(true), Value::Bool(true)],
        [Value::Null, Value::Bool(true), Value::Null],
    ];
    let expected_not = [Value::Bool(true), Value::Bool(false), Value::Null];

    for (left_index, left) in values.iter().enumerate() {
        assert_eq!(eval(&format!("NOT {left}")), expected_not[left_index]);
        for (right_index, right) in values.iter().enumerate() {
            assert_eq!(
                eval(&format!("{left} OR {right}")),
                expected_or[left_index][right_index],
                "{left} OR {right}"
            );
        }
    }
}

#[test]
fn null_propagates_except_for_null_aware_constructs() {
    for sql in [
        "NULL + 2",
        "10 * NULL",
        "NULL = NULL",
        "NULL < 3",
        "CAST(NULL AS STRING)",
        "lower(NULL)",
        "concat('x', NULL)",
    ] {
        assert_eq!(eval(sql), Value::Null, "{sql}");
    }

    assert_eq!(eval("NULL IS NULL"), Value::Bool(true));
    assert_eq!(eval("NULL IS NOT NULL"), Value::Bool(false));
    assert_eq!(eval("0 IS NULL"), Value::Bool(false));
    assert_eq!(eval("coalesce(NULL, NULL, 'present')"), "present".into());
}

#[test]
fn precedence_and_arithmetic_are_sql_shaped() {
    assert_eq!(eval("2 + 3 * 4"), 14_i64.into());
    assert_eq!(eval("(2 + 3) * 4"), 20_i64.into());
    assert_eq!(eval("7 / 2"), 3.5_f64.into());
    assert_eq!(eval("7 % 3"), 1_i64.into());
    assert_eq!(eval("2 + 0.5"), 2.5_f64.into());
    assert_eq!(eval("NOT 2 + 1 = 3"), Value::Bool(false));
}

#[test]
fn integer_boundaries_and_numeric_faults_are_explicit() {
    assert_eq!(eval("9223372036854775807"), i64::MAX.into());
    assert_eq!(eval("-9223372036854775808"), i64::MIN.into());

    for sql in [
        "9223372036854775807 + 1",
        "-9223372036854775808 - 1",
        "-(-9223372036854775808)",
        "-9223372036854775808 % -1",
    ] {
        assert!(matches!(eval_error(sql), Error::Overflow { .. }), "{sql}");
    }
    for sql in ["1 / 0", "1.0 / -0.0", "1 % 0"] {
        assert_eq!(eval_error(sql), Error::DivideByZero, "{sql}");
    }
    assert_eq!(eval("NULL / 0"), Value::Null);

    let Value::Float64(infinity) = eval("1e308 * 1e308") else {
        panic!("floating arithmetic must remain Float64");
    };
    assert!(infinity.is_infinite());
}

#[test]
fn comparisons_support_numeric_promotion_and_reject_unrelated_types() {
    assert_eq!(eval("2 = 2.0"), Value::Bool(true));
    assert_eq!(eval("2 < 2.5"), Value::Bool(true));
    assert_eq!(eval("'abc' < 'abd'"), Value::Bool(true));
    assert_eq!(eval("FALSE < TRUE"), Value::Bool(true));
    assert!(matches!(eval_error("1 = '1'"), Error::Type { .. }));
}

#[test]
fn nan_is_non_null_and_follows_ieee_comparisons() {
    let context = EvaluationContext::new().with_value("nan", f64::NAN);
    assert_eq!(
        evaluate("nan IS NULL", &context).unwrap(),
        Value::Bool(false)
    );
    assert_eq!(evaluate("nan = nan", &context).unwrap(), Value::Bool(false));
    assert_eq!(evaluate("nan <> nan", &context).unwrap(), Value::Bool(true));
    assert_eq!(evaluate("nan < 1", &context).unwrap(), Value::Bool(false));
    let Value::Float64(value) = evaluate("nan + 1", &context).unwrap() else {
        panic!("NaN arithmetic must be Float64");
    };
    assert!(value.is_nan());
}

#[test]
fn casts_are_strict_and_cover_the_scalar_type_matrix() {
    assert_eq!(eval("CAST(' -42 ' AS INT64)"), (-42_i64).into());
    assert_eq!(eval("CAST('1.25' AS DOUBLE)"), 1.25_f64.into());
    assert_eq!(eval("CAST('false' AS BOOL)"), Value::Bool(false));
    assert_eq!(eval("CAST(TRUE AS INT)"), 1_i64.into());
    assert_eq!(eval("CAST(3.9 AS INT64)"), 3_i64.into());
    assert_eq!(eval("CAST(12 AS STRING)"), "12".into());

    for sql in ["CAST('12x' AS INT64)", "CAST('maybe' AS BOOL)"] {
        assert!(
            matches!(eval_error(sql), Error::InvalidCast { .. }),
            "{sql}"
        );
    }
    for sql in [
        "CAST(CAST('NaN' AS FLOAT64) AS INT64)",
        "CAST(CAST('Infinity' AS FLOAT64) AS INT64)",
        "CAST(9223372036854775808.0 AS INT64)",
    ] {
        assert!(
            matches!(eval_error(sql), Error::InvalidCast { .. }),
            "{sql}"
        );
    }
    let nan = Value::Float64(f64::NAN);
    assert!(matches!(
        nan.cast_to(DataType::Int64),
        Err(Error::InvalidCast { .. })
    ));
    assert!(matches!(
        Value::Float64(f64::INFINITY).cast_to(DataType::Int64),
        Err(Error::InvalidCast { .. })
    ));
}

#[test]
fn case_and_coalesce_are_lazy_and_null_aware() {
    assert_eq!(
        eval("CASE WHEN NULL THEN 1 WHEN 2 > 1 THEN 2 ELSE 3 END"),
        2_i64.into()
    );
    assert_eq!(
        eval("CASE 'b' WHEN 'a' THEN 1 WHEN 'b' THEN 2 END"),
        2_i64.into()
    );
    assert_eq!(eval("CASE NULL WHEN NULL THEN 1 ELSE 2 END"), 2_i64.into());
    assert_eq!(eval("CASE WHEN FALSE THEN 1 END"), Value::Null);
    assert_eq!(eval("CASE WHEN TRUE THEN 7 ELSE 1 / 0 END"), 7_i64.into());
    assert_eq!(eval("coalesce(4, 1 / 0)"), 4_i64.into());
    assert_eq!(eval("FALSE AND 1 / 0 = 0"), Value::Bool(false));
    assert_eq!(eval("TRUE OR 1 / 0 = 0"), Value::Bool(true));
}

#[test]
fn core_string_functions_are_unicode_aware() {
    assert_eq!(eval("length('é🙂')"), 2_i64.into());
    assert_eq!(eval("char_length('rust')"), 4_i64.into());
    assert_eq!(eval("lower('RuSt')"), "rust".into());
    assert_eq!(eval("upper('straße')"), "STRASSE".into());
    assert_eq!(eval("trim('  x  ')"), "x".into());
    assert_eq!(eval("ltrim('  x  ')"), "x  ".into());
    assert_eq!(eval("rtrim('  x  ')"), "  x".into());
    assert_eq!(eval("concat('a', '', 'b')"), "ab".into());
    assert_eq!(eval("substring('é🙂xyz', 2, 3)"), "🙂xy".into());
    assert_eq!(eval("substr('rusthouse', 5)"), "house".into());
    assert!(matches!(
        eval_error("substring('x', 0)"),
        Error::InvalidArgument { .. }
    ));
}

#[test]
fn identifiers_and_literals_work_at_the_public_sql_boundary() {
    let context = EvaluationContext::new()
        .with_value("price", 12_i64)
        .with_value("discount", Value::Null)
        .with_value("Name", "O'Brien");
    assert_eq!(
        evaluate("PRICE - coalesce(discount, 2)", &context).unwrap(),
        10_i64.into()
    );
    assert_eq!(
        evaluate("name = 'O''Brien'", &context).unwrap(),
        Value::Bool(true)
    );
    assert!(matches!(
        evaluate("missing + 1", &context),
        Err(Error::UnknownColumn(name)) if name == "missing"
    ));
    parse("case when true then 'ok' end").unwrap();
}

#[test]
fn quoted_identifiers_are_never_reclassified_as_keywords() {
    let context = EvaluationContext::new()
        .with_value("NULL", 11_i64)
        .with_value("TRUE", 12_i64)
        .with_value("CASE", 13_i64)
        .with_value("odd\"name", 14_i64);

    assert_eq!(evaluate("\"NULL\"", &context).unwrap(), 11_i64.into());
    assert_eq!(evaluate("\"TRUE\"", &context).unwrap(), 12_i64.into());
    assert_eq!(evaluate("\"CASE\"", &context).unwrap(), 13_i64.into());
    assert_eq!(
        evaluate("\"odd\"\"name\"", &context).unwrap(),
        14_i64.into()
    );
}

#[test]
fn excessive_expression_depth_returns_an_error_instead_of_overflowing_the_stack() {
    let allowed = format!("{}1", "+".repeat(MAX_EXPRESSION_DEPTH - 1));
    assert_eq!(
        evaluate(&allowed, &EvaluationContext::new()).unwrap(),
        Value::Int64(1)
    );

    let unary = format!("{}1", "+".repeat(10_000));
    assert_eq!(
        evaluate(&unary, &EvaluationContext::new()),
        Err(Error::ExpressionTooDeep {
            limit: MAX_EXPRESSION_DEPTH
        })
    );

    let left_deep = std::iter::repeat_n("1", 10_000)
        .collect::<Vec<_>>()
        .join("+");
    assert_eq!(
        parse(&left_deep),
        Err(Error::ExpressionTooDeep {
            limit: MAX_EXPRESSION_DEPTH
        })
    );

    let mut direct = Expr::Literal(Value::Int64(1));
    for _ in 0..MAX_EXPRESSION_DEPTH {
        direct = Expr::Unary {
            operator: UnaryOperator::Plus,
            expression: Box::new(direct),
        };
    }
    assert_eq!(
        direct.evaluate(&EvaluationContext::new()),
        Err(Error::ExpressionTooDeep {
            limit: MAX_EXPRESSION_DEPTH
        })
    );
}

#[test]
fn malformed_expressions_return_positioned_parse_errors() {
    for sql in ["", "1 +", "CAST(1 AS UNKNOWN)", "CASE END", "'open"] {
        assert!(matches!(parse(sql), Err(Error::Parse { .. })), "{sql}");
    }
}

fn eval_error(sql: &str) -> Error {
    evaluate(sql, &EvaluationContext::new()).unwrap_err()
}
