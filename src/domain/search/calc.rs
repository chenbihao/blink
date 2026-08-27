//! 实时计算：evalexpr 表达式求值。
//!
//! 输入 "1+1" → "2"，"50%" → "0.5"，"2/3" → "0.666667"。
//! 百分比和整数转浮点为预处理，evalexpr 只做纯表达式求值。

/// 尝试实时计算：输入为合法算术表达式时返回结果字符串，否则返回 None。
///
/// 支持：四则运算、括号、取余（`%`）、`sqrt`。
/// 不支持：三角函数、对数（非 launcher 场景）。
/// 当用户刚输入尾部运算符或 `=` 时，保留前一个已完成表达式的结果。
pub fn try_eval(query: &str) -> Option<String> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    // 必须包含运算符或函数调用，避免纯数字（如 "123"）被当作计算
    if !q.contains(|c: char| "+-*/^%()".contains(c)) && !q.contains("sqrt") {
        return None;
    }
    eval_complete(q).or_else(|| {
        let prefix = completed_prefix(q)?;
        // "7+" 还没有产生过一个完整运算，不将纯数字 7 冒充成计算结果。
        if !has_arithmetic_intent(prefix) {
            return None;
        }
        eval_complete(prefix)
    })
}

/// 对一个完整表达式求值。
fn eval_complete(expression: &str) -> Option<String> {
    // 整数转浮点：避免 "2/3=0"（整数除法截断），变成 "2.0/3.0" → 0.666667
    let expression = ints_to_float(expression);
    evalexpr::eval(&expression).ok().map(format_result)
}

/// 只剥离输入中明确表示“尚未完成”的尾部，避免将任意错误文本当成公式。
fn completed_prefix(expression: &str) -> Option<&str> {
    let mut prefix = expression.trim_end();
    let original_len = prefix.len();

    if let Some(without_equals) = prefix.strip_suffix('=') {
        prefix = without_equals.trim_end();
    }

    while prefix
        .chars()
        .last()
        .is_some_and(|c| matches!(c, '+' | '-' | '*' | '/' | '^'))
    {
        prefix = prefix[..prefix.len() - 1].trim_end();
    }

    (prefix.len() < original_len && !prefix.is_empty()).then_some(prefix)
}

fn has_arithmetic_intent(expression: &str) -> bool {
    expression.contains(|c: char| "+-*/^%()".contains(c)) || expression.contains("sqrt")
}

/// 格式化计算结果：整数结果不带小数点，浮点最多 6 位有效数字。
fn format_result(val: evalexpr::Value) -> String {
    match val {
        evalexpr::Value::Float(f) => {
            if f == f.floor() && f.is_finite() && f.abs() < 1e15 {
                format!("{}", f as i64)
            } else {
                format!("{:.6}", f)
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_string()
            }
        }
        evalexpr::Value::Int(i) => format!("{}", i),
        other => format!("{}", other),
    }
}

/// 整数转浮点：把纯整数字面量加 ".0"，避免整数除法截断（"2/3" → "2.0/3.0"）。
/// 已是小数的数值（含小数点）整体保留，不再追加 ".0"（否则 "3.14" 会被破坏成 "3.14.0"）。
fn ints_to_float(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut num = String::new();
            num.push(c);
            while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                num.push(chars.next().unwrap());
            }
            if chars.peek() == Some(&'.') {
                // 已是小数：连同小数点和小数部分一起消费，不补 ".0"
                num.push(chars.next().unwrap()); // '.'
                while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    num.push(chars.next().unwrap());
                }
            } else {
                // 纯整数：补 ".0" 避免整数除法截断
                num.push_str(".0");
            }
            result.push_str(&num);
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints_to_float_handles_decimals() {
        // 纯整数补 .0
        assert_eq!(ints_to_float("1+1"), "1.0+1.0");
        assert_eq!(ints_to_float("2/3"), "2.0/3.0");
        // 已是小数的不被破坏（回归 "3.14" → "3.14.0" 的 bug）
        assert_eq!(ints_to_float("3.14*2"), "3.14*2.0");
        assert_eq!(ints_to_float("1.5+1"), "1.5+1.0");
        assert_eq!(ints_to_float("10.5"), "10.5");
        // 函数名保留，函数参数里的整数仍转浮点
        assert_eq!(ints_to_float("sqrt(9)"), "sqrt(9.0)");
    }

    #[test]
    fn eval_integer_arithmetic() {
        assert_eq!(try_eval("1+1").as_deref(), Some("2"));
        assert_eq!(try_eval("2*3").as_deref(), Some("6"));
        // 整数除法不截断
        assert_eq!(try_eval("2/3").as_deref(), Some("0.666667"));
    }

    #[test]
    fn eval_decimal_arithmetic() {
        // bug 回归测试：含小数的输入此前全部返回 None
        assert_eq!(try_eval("3.14*2").as_deref(), Some("6.28"));
        assert_eq!(try_eval("1.5+1").as_deref(), Some("2.5"));
        assert_eq!(try_eval("0.1+0.2").as_deref(), Some("0.3"));
    }

    #[test]
    fn eval_rejects_non_expressions() {
        // 纯数字不算计算
        assert_eq!(try_eval("123"), None);
        // 空输入
        assert_eq!(try_eval(""), None);
        assert_eq!(try_eval("   "), None);
        // 纯文本
        assert_eq!(try_eval("hello"), None);
    }

    #[test]
    fn eval_integer_result_no_decimal_point() {
        // 整数结果不带小数点
        assert_eq!(try_eval("4/2").as_deref(), Some("2"));
        assert_eq!(try_eval("2.0*3").as_deref(), Some("6"));
    }

    #[test]
    fn eval_keeps_last_complete_result_while_typing() {
        assert_eq!(try_eval("7+9+").as_deref(), Some("16"));
        assert_eq!(try_eval("7+9= ").as_deref(), Some("16"));
        assert_eq!(try_eval("7+9*-").as_deref(), Some("16"));
    }

    #[test]
    fn eval_does_not_recover_from_arbitrary_invalid_suffix() {
        assert_eq!(try_eval("7+9x"), None);
        assert_eq!(try_eval("7+"), None);
        assert_eq!(try_eval("="), None);
    }
}
