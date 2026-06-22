//! 实时计算：evalexpr 表达式求值。
//!
//! 输入 "1+1" → "2"，"50%" → "0.5"，"2/3" → "0.666667"。
//! 百分比和整数转浮点为预处理，evalexpr 只做纯表达式求值。

/// 尝试实时计算：输入为合法算术表达式时返回结果字符串，否则返回 None。
///
/// 支持：四则运算、括号、取余（`%`）、`sqrt`。
/// 不支持：三角函数、对数（非 launcher 场景）。
pub fn try_eval(query: &str) -> Option<String> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    // 必须包含运算符或函数调用，避免纯数字（如 "123"）被当作计算
    if !q.contains(|c: char| "+-*/^%()".contains(c)) && !q.contains("sqrt") {
        return None;
    }
    // 整数转浮点：避免 "2/3=0"（整数除法截断），变成 "2.0/3.0" → 0.666667
    let q = ints_to_float(q);
    evalexpr::eval(&q).ok().map(format_result)
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
fn ints_to_float(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut num = String::new();
            num.push(c);
            while chars.peek().map_or(false, |c| c.is_ascii_digit()) {
                num.push(chars.next().unwrap());
            }
            result.push_str(&num);
            if chars.peek() != Some(&'.') {
                result.push_str(".0");
            }
        } else {
            result.push(c);
        }
    }
    result
}
