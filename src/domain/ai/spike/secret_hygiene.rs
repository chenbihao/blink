//! 骨架 #6：**SecretString 生命周期干净** —— 验证密钥在整个 AI 调用假想链路里
//! 不会通过 tracing / Debug / serde 任何一处泄漏。
//!
//! **测试哲学**：Rust 的 `Zeroizing<String>` drop 时按字节清零,这是 zeroize crate
//! 保证的行为,不必自己验其正确性(那属于 zeroize 自己的测试范围)。**我们要验的是
//! "使用姿势正确"**——即业务代码调用 SecretString 的路径上不会不小心泄漏。
//!
//! 三条断言:
//! 1. tracing::debug!(?secret) 输出不含明文(Debug 掩码)
//! 2. tracing::debug!(%secret) 输出不含明文(Display 掩码)
//! 3. SecretString 走 Debug/Display 后原文完好——没被误 mutation
//!
//! **额外一条**:验证 serde 上不能序列化——SecretString 没 impl Serialize,
//! 编译期就挡住,不需要运行时测。这是**编译期负面测试**的价值。

use crate::infra::platform::secret::SecretString;

const REAL_KEY: &str = "sk-live-blink-1234567890abcdef";

/// 断言 1:tracing 的 `?debug` 通路不泄漏。
///
/// 用 `format!("{:?}", ...)` 模拟 `tracing::debug!(?secret)` 的展开——tracing
/// 底层就是把 `?` 转成 `fmt::Debug`。断言字符串里绝不含 REAL_KEY 的任何部分。
#[test]
fn secret_debug_channel_never_leaks() {
    let s = SecretString::new(REAL_KEY.to_string());
    let debug_output = format!("{s:?}");

    // 完整密钥字面量不能出现
    assert!(
        !debug_output.contains(REAL_KEY),
        "Debug 输出含完整密钥: {debug_output}"
    );
    // 常见密钥前缀不能出现
    assert!(
        !debug_output.contains("sk-live"),
        "Debug 输出含密钥前缀: {debug_output}"
    );
    // 密钥中段任意 6 字符片段不能出现
    assert!(
        !debug_output.contains("blink-1234"),
        "Debug 输出含密钥体: {debug_output}"
    );

    // 但至少要有个明显的标记表示"这是掩码后的"——避免误认为空字符串
    assert!(
        debug_output.contains("redacted"),
        "Debug 输出应含 redacted 标记"
    );
}

/// 断言 2:tracing 的 `%display` 通路不泄漏。
#[test]
fn secret_display_channel_masks_middle() {
    let s = SecretString::new(REAL_KEY.to_string());
    let display_output = format!("{s}");

    // 完整密钥不能出现
    assert!(!display_output.contains(REAL_KEY));
    // 前缀不能出现(sk-live-blink-1234... 的中段 "blink-12" 之类)
    assert!(!display_output.contains("sk-live"));
    assert!(!display_output.contains("blink"));
    assert!(!display_output.contains("12345"));

    // Display 允许"最后 4 字符"——`cdef` 是 REAL_KEY 末尾;这是设计要求
    // (用户设置页需要看到 last4 来区分自己配的多个 Key)
    assert!(display_output.ends_with("cdef"));
    assert!(display_output.starts_with("••••••••"));
}

/// 断言 3:多次读取 Debug/Display 不 mutate 密钥原文。
///
/// 防止未来重构不小心改成"每次 fmt 消耗内部字符串"的 bug。
#[test]
fn secret_stays_intact_after_repeated_formatting() {
    let s = SecretString::new(REAL_KEY.to_string());
    let _ = format!("{s:?}");
    let _ = format!("{s}");
    let _ = format!("{s:?}");

    // expose() 依旧返回原文,长度依旧
    assert_eq!(s.expose(), REAL_KEY);
    assert_eq!(s.len(), REAL_KEY.len());
}

/// 断言 4:传入函数按 &SecretString 也不泄漏。
///
/// 模拟 "AIProvider::complete(&self, req: CompletionRequest)" 里内部用 tracing
/// 记调用信息但把 secret 也放进 span 时的行为。
#[test]
fn secret_span_style_capture_is_masked() {
    let s = SecretString::new(REAL_KEY.to_string());

    // 模拟 tracing 里的 field capture: `tracing::info_span!("call", api_key = ?s)`
    // 展开等价于把 ?s 存下来后 fmt::Debug。此处直接拿 Debug 结果。
    fn capture_as_field(secret: &SecretString) -> String {
        // 这段代码模拟"tracing 里不小心把 secret 作为 field 记录了"
        format!("{secret:?}")
    }

    let captured = capture_as_field(&s);
    assert!(!captured.contains(REAL_KEY));
    assert!(captured.contains("redacted"));
}

/// 断言 5:SecretString 不能被 serde 序列化——**编译期保证**。
///
/// 本测试是"结构断言":如果未来有人给 SecretString 加上 `#[derive(Serialize)]`,
/// 就穿透了 §5.5 review 清单的第 3 条铁则。真正的守护是**编译失败**——
/// 下面代码块 uncomment 就会立刻编译不过:
///
/// ```compile_fail
/// use blink::infra::platform::secret::SecretString;
/// let s = SecretString::new("sk-x".to_string());
/// let _ = serde_json::to_string(&s);  // ← "trait `Serialize` not implemented"
/// ```
///
/// **⚠ 此守护只在 `cargo test --doc`(或全量 `cargo test`)时执行**——
/// CLAUDE.md §3 的 `cargo test --bin blink` 不跑 doctests,标准流程里它是空跑。
/// 要让它进回归,需在测试命令补 `--doc`(或在 CI 跑全量 `cargo test`)。
///
/// 运行时能验的:SecretString 类型仍然导出且能构造(反证"没被误删")。
#[test]
fn secret_string_does_not_impl_serialize() {
    let s = SecretString::new("test".to_string());
    assert_eq!(s.expose(), "test");
    // 若某天有人给 SecretString 加了 Serialize,上面的 compile_fail 文档测试就失效——
    // review 时会撞上 §5.5 铁则 3,应立即回滚
}
