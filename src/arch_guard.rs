//! 分层守卫（0.22 D2）——随 `cargo test --bin blink` 运行的架构守卫。
//!
//! 背景：domain 层测试曾引用 crate::app::command_error::CommandError，
//! 违反分层依赖方向。手动修复后需要自动化守卫防止复发。
//! 原为 xtask release 预检的一项，2026-09 迁入此处：守卫对象是代码结构，
//! 任何一次普通 commit 都可能违反，应随常规测试运行，而非等到发版才暴露。
//!
//! 规则：
//! - src/domain/** 禁止引用 crate::app 和 tauri::（包括 #[cfg(test)] 内）
//! - src/infra/** 禁止引用 crate::app
//! - 不禁止 infra 的平台实现使用 Tauri（infra/platform/window/windows.rs 属允许场景）
//!
//! 实现方式：轻量 Rust 源码逐行扫描（非 AST，但覆盖常见路径模式）：
//! - 单行和多行 use
//! - alias/grouped import
//! - 全限定路径 crate::app::x::Y::new()
//! - #[cfg(test)] 模块内仍能命中（不跳过 test 模块）
//! - 注释行和行内注释剥离（避免误报）
//! - 字符串字面量剥离（避免误报）
//! - 合法的 domain→infra/domain 不误报

use std::path::{Path, PathBuf};

/// 对仓库 src/domain 与 src/infra 执行分层守卫扫描，返回违规列表（空 = 通过）。
fn check_repo_layers() -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = root.join("src");
    let mut failures = Vec::new();

    check_layer_for_dir(
        &src.join("domain"),
        &["crate::app", "tauri::"],
        "domain",
        &mut failures,
    );
    check_layer_for_dir(&src.join("infra"), &["crate::app"], "infra", &mut failures);

    failures
}

/// 检查目录下所有 .rs 文件是否包含禁止的路径引用。
///
/// 扫描策略（逐行处理，避免误报）：
/// 1. 跳过注释行（`//`、`///`、`/*`、`*/`）
/// 2. 剥离行内注释（`//` 之后部分，但不处理字符串内的 `//`）
/// 3. 剥离字符串字面量（`"..."` 中的内容替换为空）
/// 4. 在剩余的纯代码文本中搜索禁止的路径模式
///
/// **不跳过 `#[cfg(test)]` 模块**——Handoff D2 要求 cfg(test) 内仍能命中。
/// 测试中的架构自测使用 `format!` 构造禁止路径字符串，字符串剥离后不会误报。
///
/// **不使用 AST**——轻量文本扫描，覆盖常见 use/path 模式，
/// 假阳性由注释剥离 + 字符串剥离两层过滤控制。
fn check_layer_for_dir(
    dir: &Path,
    forbidden_paths: &[&str],
    layer_name: &str,
    failures: &mut Vec<String>,
) {
    let mut rs_files = Vec::new();
    collect_rust_files(dir, &mut rs_files);

    for file in &rs_files {
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };

        for (line_num, raw_line) in content.lines().enumerate() {
            let trimmed = raw_line.trim_start();

            // 跳过注释行
            if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed == "*/" {
                continue;
            }

            // 剥离行内注释和字符串字面量
            let cleaned = strip_strings_and_comments(raw_line);

            for forbidden in forbidden_paths {
                if cleaned.contains(forbidden) {
                    let rel = file
                        .strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")))
                        .unwrap_or(file)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let trimmed_line = raw_line.trim();
                    failures.push(format!(
                        "分层守卫违反: {layer_name} 层文件 {rel}:{} 引用了 {forbidden}\n  > {trimmed_line}",
                        line_num + 1
                    ));
                }
            }
        }
    }
}

/// 剥离一行中的字符串字面量和行内注释。
///
/// 简化处理：不跟踪嵌套字符串状态（跨行字符串不处理），
/// 对单行内的 `"..."` 替换为空，`//` 之后内容删除。
/// 这对 use 语句和路径引用检测足够——这些不会出现在字符串内。
fn strip_strings_and_comments(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut in_string = false;
    let mut prev_char = '\0';
    let chars = line.chars();

    for ch in chars {
        if in_string {
            if ch == '"' && prev_char != '\\' {
                in_string = false;
                // 不追加字符串内容
            }
            prev_char = ch;
            continue;
        }

        // 检测行内注释开始（// 但不在字符串内）
        if ch == '/' && prev_char == '/' {
            // 去掉已追加的前一个 '/'
            result.pop();
            break;
        }

        if ch == '"' {
            in_string = true;
            prev_char = ch;
            continue;
        }

        result.push(ch);
        prev_char = ch;
    }

    result
}

/// 递归收集 .rs 文件。
fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{check_layer_for_dir, check_repo_layers, strip_strings_and_comments};
    use std::fs;
    use std::path::PathBuf;

    /// 创建临时目录并写入内容，返回目录路径。
    fn make_tmp_dir(contents: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blink-layer-guard-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        for (name, content) in contents {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }
        dir
    }

    /// 清理临时目录。
    fn cleanup(dir: &PathBuf) {
        let _ = fs::remove_dir_all(dir);
    }

    // ── 全仓守卫本体 ──────────────────────────────────────────────────────

    /// 全仓分层守卫：src/domain 与 src/infra 不得违反分层依赖方向。
    /// 违反分层依赖方向的提交在这里被拦下（随 `cargo test --bin blink` 运行）。
    #[test]
    fn repo_layer_guards_pass() {
        let failures = check_repo_layers();
        assert!(
            failures.is_empty(),
            "分层依赖方向被违反，共 {} 处:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    // ── 正向 fixture：合法代码不应被误报 ──────────────────────────────────

    #[test]
    fn legitimate_use_not_flagged() {
        let dir = make_tmp_dir(&[
            (
                "a.rs",
                "use crate::infra::local_engine::process::ManagedProcess;\n",
            ),
            ("b.rs", "use crate::domain::stt::SttEngineConnection;\n"),
            ("c.rs", "// use crate::app::something;\n"),
            ("d.rs", "let s = \"crate::app::foo\";\n"),
            ("e.rs", "use tauri::Manager;\n"),
            (
                "f.rs",
                "// comment about crate::app::command_error::CommandError\n",
            ),
        ]);

        let mut failures = Vec::new();
        check_layer_for_dir(
            &dir,
            &["crate::app", "tauri::"],
            "test_domain",
            &mut failures,
        );

        // e.rs 的 tauri 应被标记
        let tauri_failures: Vec<_> = failures.iter().filter(|f| f.contains("tauri")).collect();
        assert_eq!(
            tauri_failures.len(),
            1,
            "应只有 1 个 tauri 违规，实际: {tauri_failures:?}"
        );
        // 注释和字符串中的 crate::app 不应被误报
        let app_failures: Vec<_> = failures
            .iter()
            .filter(|f| f.contains("crate::app"))
            .collect();
        assert!(
            app_failures.is_empty(),
            "注释和字符串中的 crate::app 不应被误报: {app_failures:?}"
        );

        cleanup(&dir);
    }

    #[test]
    fn grouped_import_detected() {
        let dir = make_tmp_dir(&[(
            "a.rs",
            "use crate::app::{command_error::CommandError, other};\n",
        )]);

        let mut failures = Vec::new();
        check_layer_for_dir(&dir, &["crate::app"], "test_domain", &mut failures);
        assert_eq!(failures.len(), 1, "grouped import 应被检测到: {failures:?}");

        cleanup(&dir);
    }

    #[test]
    fn aliased_import_detected() {
        let dir = make_tmp_dir(&[("a.rs", "use crate::app as app_layer;\n")]);

        let mut failures = Vec::new();
        check_layer_for_dir(&dir, &["crate::app"], "test_domain", &mut failures);
        assert!(
            !failures.is_empty(),
            "aliased import 应被检测到: {failures:?}"
        );

        cleanup(&dir);
    }

    #[test]
    fn fully_qualified_path_detected() {
        let dir = make_tmp_dir(&[(
            "a.rs",
            "let err = crate::app::command_error::CommandError::new();\n",
        )]);

        let mut failures = Vec::new();
        check_layer_for_dir(&dir, &["crate::app"], "test_domain", &mut failures);
        assert_eq!(failures.len(), 1, "全限定路径应被检测到: {failures:?}");

        cleanup(&dir);
    }

    // ── 负向 fixture：cfg(test) 内的违规应被命中 ───────────────────────────

    #[test]
    fn cfg_test_violation_is_caught() {
        let dir = make_tmp_dir(&[(
            "test.rs",
            "#[cfg(test)]\nmod tests {\n    use crate::app::CommandError;\n}\n",
        )]);

        let mut failures = Vec::new();
        check_layer_for_dir(&dir, &["crate::app"], "test_domain", &mut failures);
        assert_eq!(
            failures.len(),
            1,
            "cfg(test) 内的 crate::app 引用应被命中: {failures:?}"
        );

        cleanup(&dir);
    }

    // ── 边界测试：strip_strings_and_comments ────────────────────────────────

    #[test]
    fn strip_removes_string_literals() {
        let cleaned = strip_strings_and_comments("let s = \"crate::app::foo\";");
        assert!(
            !cleaned.contains("crate::app"),
            "字符串字面量中的 crate::app 应被剥离: '{cleaned}'"
        );
    }

    #[test]
    fn strip_removes_inline_comments() {
        let cleaned = strip_strings_and_comments("use foo; // crate::app comment");
        assert!(
            !cleaned.contains("crate::app"),
            "行内注释中的 crate::app 应被剥离: '{cleaned}'"
        );
    }

    #[test]
    fn strip_preserves_real_paths() {
        let cleaned = strip_strings_and_comments("use crate::app::CommandError;");
        assert!(
            cleaned.contains("crate::app"),
            "真实代码中的 crate::app 不应被剥离: '{cleaned}'"
        );
    }
}
