//! xtask — Blink 构建编排工具
//!
//! 用法：
//!   cargo xtask plugins   编译 Rust 插件并拷贝到 plugins/builtin/<id>/bin/
//!   cargo xtask release   编译插件 + cargo tauri build（本地一键打包）
//!
//! 设计动机：原方案把插件编译挂在 Tauri 的 beforeBuildCommand 钩子（其 cwd
//! 不可控）并用相对路径定位 ps1，在 CI 的 tauri-action 上下文里找不到脚本。
//! xtask 用 env!("CARGO_MANIFEST_DIR") 在编译期锚定 workspace 根，cwd 完全
//! 由代码掌控，本地与 CI 共用同一入口，不再依赖任何外部 cwd 假设。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 随 MSI 打包的 Rust 插件 id（源码在 plugins/examples/<id>/，包名 blink-plugin-<id>）。
const RUST_PLUGINS: &[&str] = &["echo", "ip", "weather"];

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

/// 编译所有 Rust 插件（release）并拷贝到 plugins/builtin/<id>/bin/。
fn build_plugins() {
    let root = workspace_root();
    let target_release = root.join("target").join("release");
    let builtin_dir = root.join("plugins").join("builtin");

    println!("🔨 编译 Rust 插件（release）...");
    for id in RUST_PLUGINS {
        let pkg = format!("blink-plugin-{id}");
        print!("  编译 {pkg} ... ");
        // -p 显式选 workspace 成员包（跨 cargo 版本稳定，详见原 copy-plugins.ps1 注释）
        run("cargo", &["build", "--release", "-p", pkg.as_str()], &root);

        let dest_dir = builtin_dir.join(id).join("bin");
        std::fs::create_dir_all(&dest_dir)
            .unwrap_or_else(|e| panic!("创建 {} 失败: {e}", dest_dir.display()));
        let dest = dest_dir.join(format!("{pkg}.exe"));
        std::fs::copy(target_release.join(format!("{pkg}.exe")), &dest)
            .unwrap_or_else(|e| panic!("拷贝 {pkg}.exe 失败: {e}"));
        println!("✓ -> {}", dest.display());
    }
    println!("🐍 脚本插件无需编译（Python/Node.js 源码已在 builtin 下）");
    println!("✅ 插件打包完成");
}

fn main() {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| panic!("用法: cargo xtask <plugins|release>"));

    match task.as_str() {
        "plugins" => build_plugins(),
        "release" => {
            build_plugins();
            let root = workspace_root();
            println!("📦 cargo tauri build ...");
            run("cargo", &["tauri", "build"], &root);
        }
        other => panic!("未知子命令: {other}\n用法: cargo xtask <plugins|release>"),
    }
}
