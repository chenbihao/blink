//! xtask — Blink 构建编排工具
//!
//! 用法：
//!   cargo xtask plugins        编译 Rust 插件（仅编译到 target/release，不复制到 bin）
//!   cargo xtask release        构建 GGUF worker + 插件 + 资源校验 + cargo tauri build
//!   cargo xtask release --debug 同上，但用 debug profile（DevTools 可用，F12 打开）
//!   cargo xtask release-check   仅运行 release 资源前置校验（不打包）
//!   cargo xtask tiptap         打包 Tiptap IIFE 产物到 frontend/vendor/（调用 Node 脚本）
//!   cargo xtask icons          拉取 Lucide 图标并生成 SVG sprite（调用 Python 脚本）
//!   cargo xtask models         从 LiteLLM 精选主流模型目录生成 resources/model_context_windows.json
//!   cargo xtask lint           前端防新增检查（CSS 禁止新增带 hex fallback 的 var()）
//!   cargo xtask funasr-worker  从锁定 FunASR 源码构建常驻 GGUF STT worker（0.22.7）
//!
//! 设计动机：原方案把插件编译挂在 Tauri 的 beforeBuildCommand 钩子（其 cwd
//! 不可控）并用相对路径定位 ps1，在 CI 的 tauri-action 上下文里找不到脚本。
//! xtask 用 env!("CARGO_MANIFEST_DIR") 在编译期锚定 workspace 根，cwd 完全
//! 由代码掌控，本地与 CI 共用同一入口，不再依赖任何外部 cwd 假设。
//!
//! 重要：只有 release 打包才复制到 plugins/builtin/<id>/bin/
//!      开发期 bin 目录不存在，避免 Tauri resources 递归扫描爆炸。
//!
//! ## release 资源校验（0.22.6.4）
//!
//! `check_release_resources()` 是 release 前置门禁，可由 `cargo xtask release-check`
//! 单独运行，也在 `cargo xtask release` 流程中自动执行。校验内容：
//!
//! 1. **嵌入脚本存在且语法正确**：所有 `include_str!` 引用的 .py 脚本必须存在且
//!    Python `compile()` 通过。
//! 2. **锁文件可解析**：`locked-requirements.txt` 每个包条目必须含 `==版本` 和
//!    至少一个 `--hash=sha256:`。
//! 3. **manifest/schema 一致**：`lock.json` 的 `$schema` 字段存在，且包含代码引用
//!    的 model id 字段。
//! 4. **必要许可存在**：项目根 `LICENSE` 和 Lucide `LICENSE.lucide.txt` 必须存在。
//! 5. **排除规则**：`resources/` 目录下不包含模型文件（.pt/.pth/.onnx/.gguf/
//!    .params）、staging/generation 子目录、venv、下载缓存或 `__pycache__`。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod funasr_worker;
mod paraformer_worker;

/// 自动发现所有 Rust 插件（扫描 plugins/examples/ 目录）。
fn discover_rust_plugins() -> Vec<String> {
    let root = workspace_root();
    let examples_dir = root.join("plugins").join("examples");

    let mut plugins = Vec::new();
    match std::fs::read_dir(&examples_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // 检查是否有 Cargo.toml（是 Rust 插件）
                    if path.join("Cargo.toml").exists() {
                        if let Some(id) = path.file_name().and_then(|n| n.to_str()) {
                            plugins.push(id.to_string());
                        }
                    }
                }
            }
        }
        Err(e) => panic!("扫描 plugins/examples/ 失败: {e}"),
    }

    plugins.sort(); // 保证顺序稳定
    plugins
}

/// workspace 根：CARGO_MANIFEST_DIR 编译期确定为 xtask 包目录，上溯一级即根。
fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p
}

/// 以 cwd 跑命令；失败则带上下文 panic（对齐项目「错误带上下文」规范）。
fn run(cmd: &str, args: &[&str], cwd: impl AsRef<Path>) {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .status()
        .unwrap_or_else(|e| panic!("启动 {cmd} 失败: {e}"));
    if !status.success() {
        panic!("{cmd} {} 失败，exit: {status}", args.join(" "));
    }
}

/// 编译所有 Rust 插件。
/// debug = true 时用 debug profile（供 `cargo xtask release --debug` 使用，DevTools 可用）。
/// copy_to_bin = true 时才拷贝到 plugins/builtin/<id>/bin/（仅打包时需要）。
fn build_plugins(copy_to_bin: bool, debug: bool) {
    let root = workspace_root();
    let profile = if debug { "debug" } else { "release" };
    let target_dir = root.join("target").join(profile);
    let builtin_dir = root.join("plugins").join("builtin");

    let rust_plugins = discover_rust_plugins();
    println!(
        "🔨 发现 {} 个 Rust 插件: {:?}",
        rust_plugins.len(),
        rust_plugins
    );
    println!("🔨 编译 Rust 插件（{profile}）...");

    for id in &rust_plugins {
        let pkg = format!("blink-plugin-{id}");
        print!("  编译 {pkg} ... ");
        // -p 显式选 workspace 成员包（跨 cargo 版本稳定，详见原 copy-plugins.ps1 注释）
        let mut args = vec!["build", "-p", pkg.as_str()];
        if !debug {
            args.push("--release");
        }
        run("cargo", &args, &root);
        println!("✓ -> target/{profile}/{pkg}.exe");

        if copy_to_bin {
            let dest_dir = builtin_dir.join(id).join("bin");
            std::fs::create_dir_all(&dest_dir)
                .unwrap_or_else(|e| panic!("创建 {} 失败: {e}", dest_dir.display()));
            let dest = dest_dir.join(format!("{pkg}.exe"));
            std::fs::copy(target_dir.join(format!("{pkg}.exe")), &dest)
                .unwrap_or_else(|e| panic!("拷贝 {pkg}.exe 失败: {e}"));
            println!("     拷贝 -> {}", dest.display());
        }
    }
    println!("🐍 脚本插件无需编译（Python/Node.js 源码已在 builtin下）");
    if copy_to_bin {
        println!("✅ 插件编译 + 拷贝到 bin 完成");
    } else {
        println!("✅ 插件编译完成（开发期无需拷贝到 bin）");
    }
}

/// 仅将已编译的插件 exe 拷贝到 plugins/builtin/<id>/bin/（不重新编译）。
/// 用于 CI: 先 `cargo xtask plugins` 编译，再 `cargo xtask copy` 拷贝，最后 `cargo tauri build`。
fn copy_plugins() {
    let root = workspace_root();
    let target_release = root.join("target").join("release");
    let builtin_dir = root.join("plugins").join("builtin");

    let rust_plugins = discover_rust_plugins();
    println!("📦 拷贝 {} 个 Rust 插件到 bin ...", rust_plugins.len());

    for id in &rust_plugins {
        let pkg = format!("blink-plugin-{id}");
        let src = target_release.join(format!("{pkg}.exe"));
        if !src.exists() {
            panic!("找不到 {pkg}.exe，请先运行 `cargo xtask plugins`");
        }
        let dest_dir = builtin_dir.join(id).join("bin");
        std::fs::create_dir_all(&dest_dir)
            .unwrap_or_else(|e| panic!("创建 {} 失败: {e}", dest_dir.display()));
        let dest = dest_dir.join(format!("{pkg}.exe"));
        std::fs::copy(&src, &dest).unwrap_or_else(|e| panic!("拷贝 {pkg}.exe 失败: {e}"));
        println!("  {pkg}.exe -> {}", dest.display());
    }
    println!("✅ 插件拷贝完成");
}

/// 从 LiteLLM 精选主流模型目录生成 resources/model_context_windows.json（调用 Python 脚本）。
///
/// 与 `cargo xtask icons`（Lucide 图标 Python 脚本）同性质——预处理产物，
/// 运行时 `include_str!` 嵌入零文件依赖。
fn fetch_models() {
    let root = workspace_root();
    let script = root
        .join("xtask")
        .join("scripts")
        .join("fetch-model-context-windows.py");
    if !script.exists() {
        panic!("找不到模型目录拉取脚本: {}", script.display());
    }
    println!("📋 从 LiteLLM 精选主流模型目录 ...");
    let (py_cmd, py_args) = which_python();
    let mut args = py_args.clone();
    args.push(script.to_str().unwrap().to_string());
    args.push(root.to_str().unwrap().to_string());
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run(py_cmd.as_str(), &args_ref, &root);
    println!("✅ 模型目录生成完成");
}

// ── release 资源前置校验（0.22.6.4）────────────────────────────────────────
//
// 背景：0.22 引入本地模型运行时，resources/ 下新增了嵌入的 Python server 脚本、
// 依赖锁文件和模型元数据。release 构建必须证明这些资源入包且可验证，
// 同时证明模型文件、staging/generation/venv/cache 不入包。
//
// 资源策略：所有运行时需要的脚本/锁/元数据通过 `include_str!` 嵌入 Rust 二进制
// （编译期保证存在），tauri.conf.json 的 bundle resources 只包含 plugins/builtin/**/*。
// 本检查在 release 前验证这些 `include_str!` 引用的文件确实存在且有效。

/// 所有通过 `include_str!` 嵌入 Rust 二进制的资源文件清单。
/// 新增嵌入资源时必须在此登记，否则 release-check 不会覆盖它。
const EMBEDDED_RESOURCES: &[(&str, &str, EmbeddedKind)] = &[
    (
        "resources/ocr/paddleocr/blink_ocr_server.py",
        "Blink PP-OCRv6 OCR Server",
        EmbeddedKind::PythonScript,
    ),
    (
        "resources/ocr/paddleocr/locked-requirements.txt",
        "PaddleOCR locked requirements",
        EmbeddedKind::LockedRequirements,
    ),
    (
        "resources/ocr/paddleocr/lock.json",
        "PaddleOCR model metadata",
        EmbeddedKind::ModelMetadata,
    ),
    (
        "resources/model_context_windows.json",
        "AI model context windows catalog",
        EmbeddedKind::JsonData,
    ),
];

/// 嵌入资源类型——决定校验策略。
#[derive(Clone, Copy)]
enum EmbeddedKind {
    PythonScript,
    LockedRequirements,
    ModelMetadata,
    JsonData,
}

/// 必须存在的许可文件清单。
const REQUIRED_LICENSES: &[(&str, &str)] = &[
    ("LICENSE", "Blink 项目根许可（MIT）"),
    (
        "frontend/assets/icons/LICENSE.lucide.txt",
        "Lucide 图标许可（ISC）",
    ),
];

/// 不应出现在 resources/ 目录下的模型文件扩展名。
/// 不应出现在 resources/ 目录下的模型文件扩展名。
/// 0.22.8-B 新增 .dll（禁止 ORT DLL 进入制品）。
const FORBIDDEN_MODEL_EXTS: &[&str] = &[".pt", ".pth", ".onnx", ".gguf", ".params", ".bin", ".dll"];

/// 不应出现在 resources/ 目录下的子目录名。
const FORBIDDEN_DIRS: &[&str] = &[
    "staging",
    "generations",
    "venv",
    "env",
    "cache",
    "download",
    "downloads",
    "models",
    "__pycache__",
];

/// 校验版本一致性：Cargo.toml 和 tauri.conf.json 的 version 必须互相一致。
///
/// 从 `Cargo.toml` 提取 `version = "..."`，从 tauri.conf.json 提取 `"version": "..."`。
/// 不可解析即失败。发布版本的真源是 Git tag（CI 构建时由 release.yml
/// "Sync version from Git tag" 步骤写入两个配置文件），故此处不校验具体版本号，
/// 只防止两个配置文件分叉（如 Cargo 0.22.6 但 Tauri 0.22.7）。
fn check_version_consistency(failures: &mut Vec<String>) {
    println!("📋 校验版本一致性（Cargo.toml ↔ tauri.conf.json）...");
    let root = workspace_root();

    // Cargo.toml 版本
    let cargo_toml = root.join("Cargo.toml");
    let Ok(cargo_content) = std::fs::read_to_string(&cargo_toml) else {
        failures.push(format!(
            "版本校验: Cargo.toml 读取失败 ({})",
            cargo_toml.display()
        ));
        return;
    };
    let cargo_version = extract_toml_version(&cargo_content).unwrap_or_else(|| {
        failures.push("版本校验: Cargo.toml 中未找到 version = \"...\" 字段".to_string());
        String::new()
    });

    // tauri.conf.json 版本
    let tauri_conf = root.join("tauri.conf.json");
    let Ok(tauri_content) = std::fs::read_to_string(&tauri_conf) else {
        failures.push(format!(
            "版本校验: tauri.conf.json 读取失败 ({})",
            tauri_conf.display()
        ));
        return;
    };
    let tauri_version = extract_json_version(&tauri_content).unwrap_or_else(|| {
        failures.push("版本校验: tauri.conf.json 中未找到 \"version\" 字段".to_string());
        String::new()
    });

    if cargo_version.is_empty() || tauri_version.is_empty() {
        return; // 错误已在上文记录
    }

    if check_versions_match(&cargo_version, &tauri_version, failures) {
        println!("  ✓ Cargo.toml = tauri.conf.json = {cargo_version}");
    }
}

/// 比较两个配置来源的版本号，分叉即记录 failure。返回是否一致（便于单元测试）。
fn check_versions_match(
    cargo_version: &str,
    tauri_version: &str,
    failures: &mut Vec<String>,
) -> bool {
    if cargo_version != tauri_version {
        failures.push(format!(
            "版本校验: Cargo.toml ({cargo_version}) ≠ tauri.conf.json ({tauri_version})"
        ));
        false
    } else {
        true
    }
}

/// 从 Cargo.toml 文本中提取 `version = "..."`（仅第一个匹配，即 [package] version）。
fn extract_toml_version(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("version") && trimmed.contains('=') {
            // 提取等号后引号内的值
            if let Some(start) = trimmed.find('"') {
                if let Some(end) = trimmed.rfind('"') {
                    if end > start {
                        return Some(trimmed[start + 1..end].to_string());
                    }
                }
            }
        }
    }
    None
}

/// 从 tauri.conf.json 文本中提取 `"version": "..."`。
fn extract_json_version(content: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;
    json.get("version")?.as_str().map(|s| s.to_string())
}

/// release 资源前置校验总入口。
///
/// 校验：版本一致性（Cargo.toml ↔ tauri.conf.json）、嵌入资源（脚本语法/锁文件/manifest/schema）、
/// 许可文件存在、GGUF worker 供应链、排除规则（无模型/staging/generation/venv/cache/__pycache__）、
/// ONNX OCR 供应链锁定。
/// 分层守卫不在发布预检（它守的是代码结构而非打包产物），已迁至 blink bin crate 的
/// `src/arch_guard.rs`，随 `cargo test --bin blink` 每次运行。
fn check_release_resources() {
    println!("🔒 release 资源前置校验开始...");
    let mut failures = Vec::new();

    // 0. 版本一致性：Cargo.toml ↔ tauri.conf.json（真源是 Git tag，CI 构建时同步写入）
    check_version_consistency(&mut failures);

    // 1. 嵌入脚本存在且语法正确 + 锁文件可解析 + manifest/schema 一致
    check_embedded_resources(&mut failures);

    // 2. 必要许可文件存在
    check_required_licenses(&mut failures);

    // 3. GGUF worker 供应链：来源锁文件与构建常量一致 + 随发布 manifest 就位
    check_gguf_worker_supply_chain(&mut failures);

    // 4. 排除规则：resources/ 下无模型/staging/generation/venv/cache/__pycache__
    check_exclusion_rules(&mut failures);

    // 5. ONNX OCR 供应链锁定校验（0.22.8-B）
    check_onnx_asset_lock(&mut failures);

    // 6. STT ParaformerOnline 供应链锁定校验（0.22.9）
    check_stt_asset_lock(&mut failures);

    if !failures.is_empty() {
        for f in &failures {
            eprintln!("❌ {f}");
        }
        panic!("release 资源校验失败：{} 个错误", failures.len());
    }
    println!("✅ release 资源前置校验全部通过");
}

/// 校验 GGUF 常驻 worker 的可复现来源锁定与发布产物（0.22.7）。
///
/// - `resources/stt/funasr-gguf/worker-lock.json`：来源锁文件存在、可解析，
///   且 funasr commit/release tag/zip sha256 与 `funasr_worker` 构建常量一致
///   （两处声明漂移即失败）；
/// - `resources/bin/funasr-worker/manifest.json`：`cargo xtask funasr-worker`
///   的构建产物清单就位（exe 不入 Git，release 前必须先构建）。
fn check_gguf_worker_supply_chain(failures: &mut Vec<String>) {
    println!("🔒 校验 GGUF worker 供应链锁定...");
    let root = workspace_root();

    let lock_path = root.join("resources/stt/funasr-gguf/worker-lock.json");
    let Ok(content) = std::fs::read_to_string(&lock_path) else {
        failures.push(format!(
            "GGUF worker 来源锁文件读取失败 ({})",
            lock_path.display()
        ));
        return;
    };
    let Ok(lock) = serde_json::from_str::<serde_json::Value>(&content) else {
        failures.push("GGUF worker 来源锁文件不是合法 JSON (worker-lock.json)".to_string());
        return;
    };

    let expect = [
        (
            "funasr_commit",
            funasr_worker::FUNASR_COMMIT,
            "FunASR commit",
        ),
        (
            "funasr_release_tag",
            funasr_worker::FUNASR_RELEASE_TAG,
            "FunASR release tag",
        ),
        (
            "funasr_source_zip_url",
            funasr_worker::FUNASR_ZIP_URL,
            "FunASR source zip URL",
        ),
        (
            "funasr_source_zip_sha256",
            funasr_worker::FUNASR_ZIP_SHA256,
            "FunASR source zip SHA-256",
        ),
        (
            "llama_cpp_commit",
            funasr_worker::LLAMA_CPP_COMMIT,
            "llama.cpp commit",
        ),
    ];
    for (key, expected, desc) in expect {
        let actual = lock.get(key).and_then(|v| v.as_str());
        if actual != Some(expected) {
            failures.push(format!(
                "GGUF worker 来源锁漂移: {desc} 期望 {expected}，锁文件为 {actual:?}"
            ));
        }
    }

    // 三个补丁与协议头文件必须在仓库内（构建输入）
    if let Some(patches) = lock.get("patches").and_then(|v| v.as_array()) {
        for p in patches {
            let Some(rel) = p.as_str() else { continue };
            if !root.join(rel).is_file() {
                failures.push(format!("GGUF worker 补丁缺失: {rel}"));
            }
        }
    }
    let header = lock.get("shared_header").and_then(|v| v.as_str());
    if let Some(rel) = header {
        if !root.join(rel).is_file() {
            failures.push(format!("GGUF worker 协议头缺失: {rel}"));
        }
    }

    // 随发布 manifest（构建产物）就位——exe 不入 Git，release 前必须先构建
    let manifest = root.join("resources/bin/funasr-worker/manifest.json");
    if !manifest.is_file() {
        failures.push(format!(
            "GGUF worker 构建产物缺失（{}）。请先运行 `cargo xtask funasr-worker`",
            manifest.display()
        ));
    }

    // 模型 URL 浮动 ref 校验——拒绝 resolve/main，要求固定 commit SHA
    if let Some(models) = lock.get("models").and_then(|v| v.as_array()) {
        for model in models {
            let model_id = model
                .get("model_id")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let file = model
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let url = model.get("url").and_then(|v| v.as_str());
            if let Some(url) = url {
                if url.contains("/resolve/main/") {
                    failures.push(format!(
                        "GGUF 模型 URL 使用浮动 ref (resolve/main): model={model_id} file={file} url={url}。\
                         请替换为 /resolve/<commit-sha>/ 固定到不可变 revision。"
                    ));
                }
                // 校验 URL 与 lock 文件中的 sha256/size_bytes 存在
                if model.get("sha256").and_then(|v| v.as_str()).is_none() {
                    failures.push(format!(
                        "GGUF 模型缺少 sha256: model={model_id} file={file}"
                    ));
                }
                if model.get("size_bytes").and_then(|v| v.as_u64()).is_none() {
                    failures.push(format!(
                        "GGUF 模型缺少 size_bytes: model={model_id} file={file}"
                    ));
                }
                if model.get("revision").and_then(|v| v.as_str()).is_none() {
                    failures.push(format!(
                        "GGUF 模型缺少 revision: model={model_id} file={file}"
                    ));
                }
            }
        }
    }
}

/// 校验所有 `include_str!` 嵌入资源：存在性 + 类型相关的内容校验。
fn check_embedded_resources(failures: &mut Vec<String>) {
    let root = workspace_root();
    let py = which_python();

    println!("📋 校验嵌入资源（{} 项）...", EMBEDDED_RESOURCES.len());
    for (rel_path, desc, kind) in EMBEDDED_RESOURCES {
        let full = root.join(rel_path);
        if !full.exists() {
            failures.push(format!("{desc}: 文件不存在 ({rel_path})"));
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&full) else {
            failures.push(format!("{desc}: 读取失败 ({rel_path})"));
            continue;
        };

        if content.is_empty() {
            failures.push(format!("{desc}: 文件为空 ({rel_path})"));
            continue;
        }

        match kind {
            EmbeddedKind::PythonScript => {
                check_python_syntax(&full, rel_path, desc, &py, failures);
            }
            EmbeddedKind::LockedRequirements => {
                check_locked_requirements(&content, desc, failures);
            }
            EmbeddedKind::ModelMetadata => {
                check_model_metadata(&content, desc, failures);
            }
            EmbeddedKind::JsonData => {
                check_json_valid(&content, desc, failures);
            }
        }
    }
}

/// Python 脚本语法校验（使用 Python `compile()`）。
fn check_python_syntax(
    full: &Path,
    _rel_path: &str,
    desc: &str,
    py: &(String, Vec<String>),
    failures: &mut Vec<String>,
) {
    let compile_cmd = format!(
        "from pathlib import Path; compile(Path(r'{}').read_text(encoding='utf-8'), '{}', 'exec')",
        full.display(),
        full.file_name().unwrap_or_default().to_string_lossy()
    );

    let (py_cmd, py_args) = py;
    let mut cmd = Command::new(py_cmd);
    cmd.args(py_args)
        .arg("-c")
        .arg(&compile_cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(workspace_root());
    let result = cmd.output();

    match result {
        Ok(out) if out.status.success() => {
            println!("  ✓ {desc} 语法正确");
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            failures.push(format!("{desc}: Python 语法错误\n{stderr}"));
        }
        Err(e) => {
            failures.push(format!("{desc}: Python 执行失败: {e}"));
        }
    }
}

/// 锁文件校验：每行包定义必须含 `==` 版本约束和至少一个 `--hash=sha256:`。
/// 空行和注释行跳过。
fn check_locked_requirements(content: &str, desc: &str, failures: &mut Vec<String>) {
    let mut package_count = 0u32;
    let mut hash_count = 0u32;
    let mut missing_hash: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // 续行（以 --hash 开头）属于上一个包
        if trimmed.starts_with("--hash") {
            hash_count += 1;
            continue;
        }
        // 包行：name==version
        if trimmed.contains("==") {
            package_count += 1;
            // 检查同行的 hash（可能在续行）
            if !trimmed.contains("--hash=sha256:") {
                // 可能 hash 在续行，先记录包名
                let pkg_name = trimmed.split("==").next().unwrap_or(trimmed);
                missing_hash.push(pkg_name.to_string());
            }
        }
    }

    // 重新扫描：对每个包行，检查后续续行是否有 --hash
    let lines: Vec<&str> = content.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx].trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("--hash") {
            idx += 1;
            continue;
        }
        if line.contains("==") {
            // 检查同行和后续续行是否有 --hash
            let mut has_hash = line.contains("--hash=sha256:");
            let mut j = idx + 1;
            while j < lines.len() && lines[j].trim().starts_with("--hash") {
                if lines[j].contains("sha256") {
                    has_hash = true;
                }
                j += 1;
            }
            if !has_hash {
                let pkg = line.split("==").next().unwrap_or(line);
                failures.push(format!("{desc}: 包 {pkg} 缺少 --hash=sha256: 校验"));
            }
        }
        idx += 1;
    }

    if package_count == 0 {
        failures.push(format!("{desc}: 锁文件未包含任何包定义"));
    } else {
        println!("  ✓ {desc}: {package_count} 个包, {hash_count} 个 hash 条目");
    }
}

/// 模型元数据 JSON 校验：$schema 字段存在，且包含 models 字段。
fn check_model_metadata(content: &str, desc: &str, failures: &mut Vec<String>) {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(json) => {
            if json.get("$schema").is_none() {
                failures.push(format!("{desc}: 缺少 $schema 字段"));
            }
            if json.get("models").is_none() {
                failures.push(format!("{desc}: 缺少 models 字段"));
            }
            println!("  ✓ {desc}: JSON 结构有效");
        }
        Err(e) => {
            failures.push(format!("{desc}: JSON 解析失败: {e}"));
        }
    }
}

/// 普通 JSON 数据校验。
fn check_json_valid(content: &str, desc: &str, failures: &mut Vec<String>) {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(_) => {
            println!("  ✓ {desc}: JSON 有效");
        }
        Err(e) => {
            failures.push(format!("{desc}: JSON 解析失败: {e}"));
        }
    }
}

/// 校验必要许可文件存在。
fn check_required_licenses(failures: &mut Vec<String>) {
    let root = workspace_root();
    println!("📋 校验许可文件（{} 项）...", REQUIRED_LICENSES.len());
    for (rel_path, desc) in REQUIRED_LICENSES {
        let full = root.join(rel_path);
        if !full.exists() {
            failures.push(format!("{desc}: 文件不存在 ({rel_path})"));
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&full) else {
            failures.push(format!("{desc}: 读取失败 ({rel_path})"));
            continue;
        };
        if content.is_empty() {
            failures.push(format!("{desc}: 文件为空 ({rel_path})"));
            continue;
        }
        println!("  ✓ {desc}");
    }
}

/// 排除规则校验：resources/ 目录下不应有模型文件、staging/generation/venv/cache/__pycache__。
fn check_exclusion_rules(failures: &mut Vec<String>) {
    let root = workspace_root();
    let resources_dir = root.join("resources");
    println!("🚫 校验排除规则（resources/ 下无模型/staging/venv/cache/__pycache__）...");

    let mut found_forbidden: Vec<String> = Vec::new();
    scan_forbidden_in_dir(&resources_dir, &resources_dir, &mut found_forbidden);

    if found_forbidden.is_empty() {
        println!("  ✓ resources/ 目录干净（无禁止文件/目录）");
    } else {
        for item in &found_forbidden {
            failures.push(format!("排除规则违反: {item}"));
        }
    }
}

/// 校验 ONNX OCR 供应链 asset-lock.json（0.22.8-B）。
///
/// - asset-lock.json 存在且可解析
/// - ORT DLL SHA-256 非空
/// - 每个模型 SHA-256 非空、size_bytes > 0
/// - 模型 SHA-256 强校验（允许 resolve/main/ URL，hash 锁定确保不可变性）
fn check_onnx_asset_lock(failures: &mut Vec<String>) {
    println!("🔒 校验 ONNX OCR asset-lock.json...");
    let root = workspace_root();
    let lock_path = root.join("resources/ocr/paddleocr-onnx/asset-lock.json");

    let Ok(content) = std::fs::read_to_string(&lock_path) else {
        failures.push(format!(
            "ONNX asset-lock.json 读取失败 ({})",
            lock_path.display()
        ));
        return;
    };

    let Ok(lock) = serde_json::from_str::<serde_json::Value>(&content) else {
        failures.push("ONNX asset-lock.json 不是合法 JSON".to_string());
        return;
    };

    // ORT version 非空
    let ort_version = lock
        .get("ort")
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str());
    if ort_version.is_none() {
        failures.push("asset-lock.json: ort.version 缺失".to_string());
    }

    // ORT files 非空，每个有 sha256 和 size_bytes
    if let Some(files) = lock
        .get("ort")
        .and_then(|v| v.get("files"))
        .and_then(|v| v.as_array())
    {
        for file in files {
            let path = file
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            if file.get("sha256").and_then(|v| v.as_str()).is_none() {
                failures.push(format!("asset-lock.json: ORT file {path} 缺少 sha256"));
            }
            if file.get("size_bytes").and_then(|v| v.as_u64()).is_none() {
                failures.push(format!("asset-lock.json: ORT file {path} 缺少 size_bytes"));
            }
        }
    } else {
        failures.push("asset-lock.json: ort.files 缺失或为空".to_string());
    }

    // models 非空，每个有 sha256、size_bytes、url
    if let Some(models) = lock.get("models").and_then(|v| v.as_array()) {
        if models.is_empty() {
            failures.push("asset-lock.json: models 为空".to_string());
        }
        for model in models {
            let filename = model
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            if model.get("sha256").and_then(|v| v.as_str()).is_none() {
                failures.push(format!("asset-lock.json: model {filename} 缺少 sha256"));
            }
            if model.get("size_bytes").and_then(|v| v.as_u64()).is_none() {
                failures.push(format!("asset-lock.json: model {filename} 缺少 size_bytes"));
            }
            if let Some(_url) = model.get("url").and_then(|v| v.as_str()) {
                // URL 允许 resolve/main/（HuggingFace 稳定 release 分支），
                // SHA-256 强校验确保不可变性，URL 格式不作为阻塞条件。
            } else {
                failures.push(format!("asset-lock.json: model {filename} 缺少 url"));
            }
        }
    } else {
        failures.push("asset-lock.json: models 缺失".to_string());
    }

    if failures.is_empty() {
        println!("  ✓ ONNX asset-lock.json 校验通过");
    }
}

/// 校验 STT ParaformerOnline 供应链 asset-lock.json（0.22.9）。
///
/// - asset-lock.json 存在且可解析
/// - ORT DLL SHA-256 和 size_bytes 非空
/// - 四个模型（encoder/decoder/cmvn/tokenizer）均存在且字段完整
/// - placeholder hash 只产生警告（不阻塞 release-check）
fn check_stt_asset_lock(failures: &mut Vec<String>) {
    println!("🔒 校验 STT asset-lock.json...");
    let root = workspace_root();
    let lock_path = root.join("resources/stt/paraformer-onnx/asset-lock.json");

    let Ok(content) = std::fs::read_to_string(&lock_path) else {
        failures.push(format!(
            "STT asset-lock.json 读取失败 ({})",
            lock_path.display()
        ));
        return;
    };

    let Ok(lock) = serde_json::from_str::<serde_json::Value>(&content) else {
        failures.push("STT asset-lock.json 不是合法 JSON".to_string());
        return;
    };

    // schema_version
    if lock.get("schema_version").and_then(|v| v.as_u64()) != Some(1) {
        failures.push("STT asset-lock.json: schema_version 缺失或不为 1".to_string());
    }

    // ORT version 非空
    let ort_version = lock
        .get("ort")
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str());
    if ort_version.is_none() {
        failures.push("STT asset-lock.json: ort.version 缺失".to_string());
    }

    // ORT files 非空，每个有 sha256 和 size_bytes
    if let Some(files) = lock
        .get("ort")
        .and_then(|v| v.get("files"))
        .and_then(|v| v.as_array())
    {
        for file in files {
            let path = file
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            if file.get("sha256").and_then(|v| v.as_str()).is_none() {
                failures.push(format!("STT asset-lock.json: ORT file {path} 缺少 sha256"));
            }
            if file.get("size_bytes").and_then(|v| v.as_u64()).is_none() {
                failures.push(format!(
                    "STT asset-lock.json: ORT file {path} 缺少 size_bytes"
                ));
            }
        }
    } else {
        failures.push("STT asset-lock.json: ort.files 缺失或为空".to_string());
    }

    // models 非空，检查必需 kind 存在
    if let Some(models) = lock.get("models").and_then(|v| v.as_array()) {
        if models.is_empty() {
            failures.push("STT asset-lock.json: models 为空".to_string());
        }

        let kinds: Vec<&str> = models
            .iter()
            .filter_map(|m| m.get("kind").and_then(|v| v.as_str()))
            .collect();

        for required in ["encoder", "decoder", "cmvn", "tokenizer"] {
            if !kinds.contains(&required) {
                failures.push(format!(
                    "STT asset-lock.json: 缺少必需的模型 kind: {required}"
                ));
            }
        }

        for model in models {
            let kind = model
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let filename = model
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");

            if model.get("sha256").and_then(|v| v.as_str()).is_none() {
                failures.push(format!(
                    "STT asset-lock.json: model {kind} ({filename}) 缺少 sha256"
                ));
            }
            if model.get("size_bytes").and_then(|v| v.as_u64()).is_none() {
                failures.push(format!(
                    "STT asset-lock.json: model {kind} ({filename}) 缺少 size_bytes"
                ));
            }
            if model.get("url").and_then(|v| v.as_str()).is_none() {
                failures.push(format!(
                    "STT asset-lock.json: model {kind} ({filename}) 缺少 url"
                ));
            }
            if model.get("license").and_then(|v| v.as_str()).is_none() {
                failures.push(format!(
                    "STT asset-lock.json: model {kind} ({filename}) 缺少 license"
                ));
            }
        }
    } else {
        failures.push("STT asset-lock.json: models 缺失".to_string());
    }

    if failures.is_empty() {
        println!("  ✓ STT asset-lock.json 校验通过");
    }
}

/// 递归扫描目录，查找禁止的文件扩展名和子目录名。
fn scan_forbidden_in_dir(base: &Path, dir: &Path, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if FORBIDDEN_DIRS.iter().any(|d| *d == name) {
                    found.push(format!("禁止目录: resources/{rel}/"));
                    // 不递归进入禁止目录
                    continue;
                }
            }
            scan_forbidden_in_dir(base, &path, found);
        } else if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext_lower = format!(".{ext}").to_lowercase();
                if FORBIDDEN_MODEL_EXTS
                    .iter()
                    .any(|e| *e == ext_lower.as_str())
                {
                    found.push(format!("禁止模型文件: resources/{rel}"));
                }
            }
            // 检查 __pycache__ 残留
            if let Some(parent) = path.parent()
                && parent.file_name().is_some_and(|n| n == "__pycache__")
            {
                found.push(format!("禁止 __pycache__ 文件: resources/{rel}"));
            }
        }
    }
}

/// 拉取 Lucide 图标并生成 SVG sprite（调用 Python 脚本）。
///
/// Python 脚本仅用标准库（urllib），由 xtask 锚定 workspace 根路径并传入，
/// 避免为一次性下载器引入 Rust HTTP 依赖。
fn fetch_icons() {
    let root = workspace_root();
    let script = root
        .join("xtask")
        .join("scripts")
        .join("fetch-lucide-icons.py");
    if !script.exists() {
        panic!("找不到图标拉取脚本: {}", script.display());
    }
    println!("🎨 拉取 Lucide 图标并生成 SVG sprite ...");
    // 优先使用 python3，回退到 python
    let (py_cmd, py_args) = which_python();
    let mut args = py_args.clone();
    args.push(script.to_str().unwrap().to_string());
    args.push(root.to_str().unwrap().to_string());
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run(py_cmd.as_str(), &args_ref, &root);
    println!("✅ 图标 sprite 生成完成");
}

/// 查找可用的 Python 解释器（python3 优先，回退 python，再回退 py launcher）。
///
/// Windows 上 `python3`/`python` 可能是 Microsoft Store 别名（不工作），
/// 因此也尝试 `py -3`（官方 Python launcher）作为最后回退。
///
/// 返回 (命令, 额外参数) 元组——调用方用 `Command::new(cmd).args(&prefix)` 构建命令。
fn which_python() -> (String, Vec<String>) {
    // 先尝试直接命令
    for cmd in &["python3", "python"] {
        // 验证能真正执行代码（Store alias 的 exit code 仍为 0 但不工作）
        let test = Command::new(cmd)
            .arg("-c")
            .arg("print('ok')")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        if let Ok(out) = test
            && out.status.success()
            && String::from_utf8_lossy(&out.stdout).trim() == "ok"
        {
            return (cmd.to_string(), vec![]);
        }
    }
    // 回退到 py launcher（Windows 官方 Python launcher）
    let test = Command::new("py")
        .arg("-3")
        .arg("-c")
        .arg("print('ok')")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    if let Ok(out) = test
        && out.status.success()
        && String::from_utf8_lossy(&out.stdout).trim() == "ok"
    {
        return ("py".to_string(), vec!["-3".to_string()]);
    }
    panic!("找不到 Python 解释器，请安装 Python 3.8+ 并确保 python/python3/py 在 PATH 中");
}

/// 打包 Tiptap IIFE 产物到 frontend/vendor/（调用 Node 脚本）。
///
/// 与 `cargo xtask icons`（Lucide 图标 Python 脚本）同性质——预处理产物，
/// 运行时零构建，不违反无 bundler 铁则（spec-frontend §1.1/§1.5）。
fn bundle_tiptap() {
    let root = workspace_root();
    let script = root.join("xtask").join("scripts").join("bundle-tiptap.js");
    if !script.exists() {
        panic!("找不到 Tiptap 打包脚本: {}", script.display());
    }
    println!("📦 打包 Tiptap IIFE 产物 ...");
    // 优先使用 node，回退到 nodejs
    let node = which_node();
    run(node.as_str(), &[script.to_str().unwrap()], &root);
    println!("✅ Tiptap 打包完成");
}

/// 查找可用的 Node.js 解释器。
fn which_node() -> String {
    for cmd in &["node", "nodejs"] {
        if Command::new(cmd)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return cmd.to_string();
        }
    }
    panic!("找不到 Node.js 解释器，请安装 Node.js 18+ 并确保 node 在 PATH 中");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let task = args.get(1).unwrap_or_else(|| {
        panic!("用法: cargo xtask <plugins|copy|release|icons|tiptap|models|lint> [--debug]")
    });

    match task.as_str() {
        "plugins" => build_plugins(false, false), // 开发期：仅编译，不复制到 bin
        "copy" => copy_plugins(),                 // CI：仅拷贝已编译的 exe 到 bin
        "release" => {
            // --debug: 用 debug profile 打包，DevTools 可用（F12 打开），用于排查多屏幕等问题
            let debug = args.iter().any(|a| a == "--debug");
            funasr_worker::build_workers(); // release 唯一入口必须自行生成 gitignore 的 worker 产物
            build_plugins(true, debug); // 打包期：编译 + 复制到 bin
            check_release_resources(); // release 资源前置校验（含 Python 语法）
            let root = workspace_root();
            if debug {
                println!("📦 cargo tauri build --debug（DevTools 可用）...");
                run("cargo", &["tauri", "build", "--debug"], &root);
            } else {
                println!("📦 cargo tauri build ...");
                run("cargo", &["tauri", "build"], &root);
            }
        }
        "release-check" => check_release_resources(), // 仅运行 release 资源前置校验
        "icons" => fetch_icons(),                     // 拉取 Lucide 图标生成 sprite
        "tiptap" => bundle_tiptap(),                  // 打包 Tiptap IIFE 产物
        "models" => fetch_models(),                   // 从 LiteLLM 精选主流模型目录
        "lint" => lint_frontend(),                    // 前端防新增检查（var hex fallback 冻结基线）
        "funasr-worker" => funasr_worker::build_workers(), // 构建 GGUF STT worker（0.22.7）
        "paraformer-worker" => paraformer_worker::build_paraformer_worker(), // 校验 ParaformerOnline 供应链（0.22.9）
        other => {
            panic!(
                "未知子命令: {other}\n用法: cargo xtask <plugins|copy|release|release-check|funasr-worker|paraformer-worker|icons|tiptap|models|lint> [--debug]"
            )
        }
    }
}

// ── CSS hex fallback 冻结基线检查（0.21.22.1）────────────────────────────
//
// 背景：--danger 幽灵 token 事故（0.21.22）——CSS 写 `var(--danger, #f38ba8)`
// 但 token 从未定义，fallback 静默生效，视觉走的是幽灵值；0.21.18 还修过
// --primary 同款。存量清洗无行为收益、膨胀 diff，不做；本检查只拦新增：
// - 未列在基线里的文件出现任何带 hex fallback 的 var() → 失败
// - 基线文件出现数超过冻结值 → 失败（减少则放行，方便渐进清偿后调低基线）

/// 各文件 hex fallback 存量冻结值（2026-08-21 基线，frontend/css/ 下相对路径）。
/// 清偿某文件的 fallback 后请同步调低/删除对应条目。
const VAR_HEX_FALLBACK_BASELINE: &[(&str, u32)] = &[
    ("components/modal.css", 1),
    ("components/voice-wave.css", 3),
    ("views/chat/bubble.css", 1),
    ("views/chat/composer.css", 1),
    ("views/chat/popup.css", 31),
    ("views/chat/tool-card.css", 3),
    ("views/chord-screenshot.css", 12),
    ("views/main-window/ai-mode.css", 4),
    ("views/main-window/result-item.css", 3),
    ("views/settings/settings-ai.css", 22),
    ("views/settings/settings-mcp.css", 2),
    ("views/settings/settings-voice.css", 1),
    ("views/settings/settings.css", 2),
    ("views/welcome.css", 12),
];

/// 一行 CSS 是否含「带 hex fallback 的 var()」：`var(--xxx, #aabbcc)`。
fn has_var_hex_fallback(line: &str) -> bool {
    let mut rest = line;
    while let Some(pos) = rest.find("var(") {
        let after = &rest[pos..];
        match after.find(')') {
            Some(close) => {
                let inner = &after[..close];
                if inner.contains("--") && inner.contains('#') {
                    return true;
                }
                rest = &after[close..];
            }
            None => return false,
        }
    }
    false
}

/// 递归收集 CSS 文件（跳过 vendor/ 第三方产物）。
fn collect_css_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "vendor") {
                continue;
            }
            collect_css_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "css") {
            out.push(path);
        }
    }
}

fn lint_frontend() {
    let root = workspace_root();
    let css_dir = root.join("frontend").join("css");
    let mut files = Vec::new();
    collect_css_files(&css_dir, &mut files);

    println!(
        "🔍 检查 CSS hex fallback（基线 {} 个文件 / 总量上限）...",
        VAR_HEX_FALLBACK_BASELINE.len()
    );
    let mut violations: Vec<String> = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&css_dir)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(content) = std::fs::read_to_string(file) else {
            violations.push(format!("{rel}: 读取失败"));
            continue;
        };
        let count = content.lines().filter(|l| has_var_hex_fallback(l)).count() as u32;
        let allowed = VAR_HEX_FALLBACK_BASELINE
            .iter()
            .find(|(p, _)| *p == rel)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        if count > allowed {
            violations.push(format!(
                "{rel}: hex fallback {count} 处，超过冻结基线 {allowed}——新代码禁止 var(--x, #hex)，改用 tokens/color.css 定义的 token"
            ));
        }
    }

    if violations.is_empty() {
        println!("✅ hex fallback 基线检查通过");
    } else {
        for v in &violations {
            eprintln!("❌ {v}");
        }
        panic!(
            "lint 失败：{} 个文件超出 hex fallback 基线",
            violations.len()
        );
    }
}

#[cfg(test)]
mod supply_chain_tests {
    /// worker-lock.json（来源锁）与 `funasr_worker` 构建常量必须一致——
    /// 两处声明漂移即失败（release-check 同规则，此处固化为单测）。
    ///
    /// 构建产物检查（resources/bin/...，gitignore 产物）在单测中放宽：
    /// 仅当本机已运行 `cargo xtask funasr-worker` 才存在。
    #[test]
    fn gguf_worker_lock_matches_build_constants() {
        let mut failures = Vec::new();
        super::check_gguf_worker_supply_chain(&mut failures);
        let repo_failures: Vec<&String> = failures
            .iter()
            .filter(|f| !f.contains("构建产物缺失"))
            .collect();
        assert!(
            repo_failures.is_empty(),
            "GGUF 供应链锁校验失败: {repo_failures:?}"
        );
    }
}

#[cfg(test)]
mod version_consistency_tests {
    use super::{extract_json_version, extract_toml_version};

    #[test]
    fn extract_toml_version_finds_package_version() {
        let toml = r#"[package]
name = "blink"
version = "0.22.7"
edition = "2024"
"#;
        assert_eq!(extract_toml_version(toml), Some("0.22.7".to_string()));
    }

    #[test]
    fn extract_toml_version_returns_none_when_missing() {
        let toml = "[package]\nname = \"blink\"\n";
        assert_eq!(extract_toml_version(toml), None);
    }

    #[test]
    fn extract_json_version_finds_version_field() {
        let json = r#"{"version": "0.22.7", "productName": "Blink"}"#;
        assert_eq!(extract_json_version(json), Some("0.22.7".to_string()));
    }

    #[test]
    fn extract_json_version_returns_none_when_missing() {
        let json = r#"{"productName": "Blink"}"#;
        assert_eq!(extract_json_version(json), None);
    }

    /// 负向测试：当 Cargo.toml 与 tauri.conf.json 版本分叉时，
    /// 必须记录 failure 且返回 false。
    #[test]
    fn version_mismatch_detected() {
        let mut failures = Vec::new();
        let ok = super::check_versions_match("0.22.6", "0.22.7", &mut failures);
        assert!(!ok, "版本分叉应返回 false");
        assert_eq!(failures.len(), 1, "版本分叉应产生恰好 1 条 failure");
        assert!(
            failures[0].contains("0.22.6") && failures[0].contains("0.22.7"),
            "failure 信息应包含两个冲突版本号: {}",
            failures[0]
        );
    }

    /// 正向测试：版本一致时返回 true 且不产生 failure。
    #[test]
    fn versions_match_passes() {
        let mut failures = Vec::new();
        let ok = super::check_versions_match("0.22.8", "0.22.8", &mut failures);
        assert!(ok, "版本一致应返回 true");
        assert!(failures.is_empty(), "版本一致不应产生 failure");
    }

    /// 正向测试：仓库中实际的 Cargo.toml 和 tauri.conf.json 版本必须一致。
    #[test]
    fn repo_versions_are_consistent() {
        let root = super::workspace_root();
        let cargo_content =
            std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml 必须可读");
        let tauri_content = std::fs::read_to_string(root.join("tauri.conf.json"))
            .expect("tauri.conf.json 必须可读");
        let cargo_v = extract_toml_version(&cargo_content).expect("Cargo.toml 必须有 version 字段");
        let tauri_v =
            extract_json_version(&tauri_content).expect("tauri.conf.json 必须有 version 字段");
        assert_eq!(
            cargo_v, tauri_v,
            "Cargo.toml 和 tauri.conf.json 版本必须一致"
        );
    }
}
