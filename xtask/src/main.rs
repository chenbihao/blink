//! xtask — Blink 构建编排工具
//!
//! 用法：
//!   cargo xtask plugins   编译 Rust 插件（仅编译到 target/release，不复制到 bin）
//!   cargo xtask release   编译插件 + 复制到 bin + cargo tauri build（本地一键打包）
//!   cargo xtask release --debug  同上，但用 debug profile（DevTools 可用，F12 打开）
//!   cargo xtask tiptap    打包 Tiptap IIFE 产物到 frontend/vendor/（调用 Node 脚本）
//!   cargo xtask icons     拉取 Lucide 图标并生成 SVG sprite（调用 Python 脚本）
//!
//! 设计动机：原方案把插件编译挂在 Tauri 的 beforeBuildCommand 钩子（其 cwd
//! 不可控）并用相对路径定位 ps1，在 CI 的 tauri-action 上下文里找不到脚本。
//! xtask 用 env!("CARGO_MANIFEST_DIR") 在编译期锚定 workspace 根，cwd 完全
//! 由代码掌控，本地与 CI 共用同一入口，不再依赖任何外部 cwd 假设。
//!
//! 重要：只有 release 打包才复制到 plugins/builtin/<id>/bin/
//!      开发期 bin 目录不存在，避免 Tauri resources 递归扫描爆炸。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    let py = which_python();
    run(
        py.as_str(),
        &[script.to_str().unwrap(), root.to_str().unwrap()],
        &root,
    );
    println!("✅ 图标 sprite 生成完成");
}

/// 查找可用的 Python 解释器（python3 优先，回退 python）。
fn which_python() -> String {
    for cmd in &["python3", "python"] {
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
    panic!("找不到 Python 解释器，请安装 Python 3.8+ 并确保 python/python3 在 PATH 中");
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
        panic!("用法: cargo xtask <plugins|copy|release|icons|tiptap> [--debug]")
    });

    match task.as_str() {
        "plugins" => build_plugins(false, false), // 开发期：仅编译，不复制到 bin
        "copy" => copy_plugins(),                 // CI：仅拷贝已编译的 exe 到 bin
        "release" => {
            // --debug: 用 debug profile 打包，DevTools 可用（F12 打开），用于排查多屏幕等问题
            let debug = args.iter().any(|a| a == "--debug");
            build_plugins(true, debug); // 打包期：编译 + 复制到 bin
            let root = workspace_root();
            if debug {
                println!("📦 cargo tauri build --debug（DevTools 可用）...");
                run("cargo", &["tauri", "build", "--debug"], &root);
            } else {
                println!("📦 cargo tauri build ...");
                run("cargo", &["tauri", "build"], &root);
            }
        }
        "icons" => fetch_icons(),    // 拉取 Lucide 图标生成 sprite
        "tiptap" => bundle_tiptap(), // 打包 Tiptap IIFE 产物
        other => {
            panic!(
                "未知子命令: {other}\n用法: cargo xtask <plugins|copy|release|icons|tiptap> [--debug]"
            )
        }
    }
}
