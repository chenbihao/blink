//! 词典数据集构建编排（0.28.4）——`cargo xtask dictionary <子操作>`。
//!
//! 委托给 `blink-plugin-dictionary` 二进制的构建 CLI：先编译该插件（debug
//! profile 即可，构建器不是热路径），再原样透传参数执行。构建实现只有一份
//! （插件 crate 内），发布流程、本地调试、质量校验共用。
//!
//! 用法（透传给插件二进制，见 DATASETS.md）：
//!
//! ```text
//! cargo xtask dictionary --build-basic --ecdict <csv> [--profile basic|full]
//!                        [--version V] [--out DIR] [--source-revision R]
//! cargo xtask dictionary --build-ety --wordroot <json> [--version V] [--out DIR]
//! cargo xtask dictionary --verify <package_dir>
//! cargo xtask dictionary --check-quality <package_dir> [--samples <json>]
//! ```

use std::process::Command;

use crate::workspace_root;

pub fn run(args: &[String]) {
    let root = workspace_root();
    // 编译词典插件（增量；构建器逻辑变更时自动重编）
    run_cargo_build(&root);
    let exe = root
        .join("target")
        .join("debug")
        .join("blink-plugin-dictionary.exe");
    if !exe.exists() {
        panic!("找不到 {}: 编译失败？", exe.display());
    }
    let status = Command::new(&exe)
        .args(args)
        .current_dir(&root)
        .status()
        .unwrap_or_else(|e| panic!("启动 {} 失败: {e}", exe.display()));
    if !status.success() {
        panic!("dictionary 子命令失败，exit: {status}（参数: {args:?}）");
    }
}

fn run_cargo_build(root: &std::path::Path) {
    let status = Command::new("cargo")
        .args(["build", "-p", "blink-plugin-dictionary"])
        .current_dir(root)
        .status()
        .unwrap_or_else(|e| panic!("启动 cargo 失败: {e}"));
    if !status.success() {
        panic!("cargo build -p blink-plugin-dictionary 失败，exit: {status}");
    }
}
