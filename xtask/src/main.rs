//! xtask — Blink 构建编排工具
//!
//! 用法：
//!   cargo xtask plugins   编译 Rust 插件（仅编译到 target/release，不复制到 bin）
//!   cargo xtask release   编译插件 + 复制到 bin + cargo tauri build（本地一键打包）
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

/// 编译所有 Rust 插件（release）。
/// copy_to_bin = true 时才拷贝到 plugins/builtin/<id>/bin/（仅 release 打包时需要）。
fn build_plugins(copy_to_bin: bool) {
    let root = workspace_root();
    let target_release = root.join("target").join("release");
    let builtin_dir = root.join("plugins").join("builtin");

    let rust_plugins = discover_rust_plugins();
    println!("🔨 发现 {} 个 Rust 插件: {:?}", rust_plugins.len(), rust_plugins);
    println!("🔨 编译 Rust 插件（release）...");

    for id in &rust_plugins {
        let pkg = format!("blink-plugin-{id}");
        print!("  编译 {pkg} ... ");
        // -p 显式选 workspace 成员包（跨 cargo 版本稳定，详见原 copy-plugins.ps1 注释）
        run("cargo", &["build", "--release", "-p", pkg.as_str()], &root);
        println!("✓ -> target/release/{pkg}.exe");

        if copy_to_bin {
            let dest_dir = builtin_dir.join(id).join("bin");
            std::fs::create_dir_all(&dest_dir)
                .unwrap_or_else(|e| panic!("创建 {} 失败: {e}", dest_dir.display()));
            let dest = dest_dir.join(format!("{pkg}.exe"));
            std::fs::copy(target_release.join(format!("{pkg}.exe")), &dest)
                .unwrap_or_else(|e| panic!("拷贝 {pkg}.exe 失败: {e}"));
            println!("     拷贝 -> {}", dest.display());
        }
    }
    println!("🐍 脚本插件无需编译（Python/Node.js 源码已在 builtin 下）");
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
        std::fs::copy(&src, &dest)
            .unwrap_or_else(|e| panic!("拷贝 {pkg}.exe 失败: {e}"));
        println!("  {pkg}.exe -> {}", dest.display());
    }
    println!("✅ 插件拷贝完成");
}

fn main() {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| panic!("用法: cargo xtask <plugins|copy|release>"));

    match task.as_str() {
        "plugins" => build_plugins(false), // 开发期：仅编译，不复制到 bin
        "copy" => copy_plugins(),          // CI：仅拷贝已编译的 exe 到 bin
        "release" => {
            build_plugins(true); // 打包期：编译 + 复制到 bin
            let root = workspace_root();
            println!("📦 cargo tauri build ...");
            run("cargo", &["tauri", "build"], &root);
        }
        other => panic!("未知子命令: {other}\n用法: cargo xtask <plugins|copy|release>"),
    }
}
