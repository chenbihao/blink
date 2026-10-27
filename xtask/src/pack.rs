//! 官方插件打包与目录生成（0.28.5 §五.5）——`cargo xtask pack`。
//!
//! 发布链路：`pack`（构建 → 插件/数据集 zip → catalog.json → 自校验）→
//! CI 上传固定 tag `plugins-catalog` 的 GitHub Release（见
//! .github/workflows/plugins-release.yml）。不套用应用 updater 的
//! latest.json 语义——插件/词库与应用版本各自独立（phase §五.5）。
//!
//! 产物布局（默认 `target/plugin-dist/`）：
//!
//! ```text
//! target/plugin-dist/
//!   blink-plugins-catalog.json
//!   builtin.translate-0.4.0-windows-x86_64.zip   # manifest.json 在 zip 根
//!   dictionary-basic-1.0.0.zip                    # dataset.json 在 zip 根
//!   ...
//! ```
//!
//! 注意（phase 验收"本地/草稿产物不等于公开发布"）：pack 产物只是候选，
//! 公开发布以 CI Release 资产为准；`--verify` 通过 ≠ 已发布。

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::workspace_root;

/// 目录文件名（与应用端 catalog_endpoint 约定一致）。
const CATALOG_FILE: &str = "blink-plugins-catalog.json";
/// 词库构建产物根（dictionary builder 默认 --out）。
const DATASET_BUILD_ROOT: &str = "target/dictionary";

/// pack 参数。
struct PackArgs {
    base: String,
    out: PathBuf,
    skip_build: bool,
    plugins_only: bool,
    ecdict: Option<String>,
    wordroot: Option<String>,
    verify_only: bool,
}

pub fn run(args: &[String]) {
    let root = workspace_root();
    let parsed = parse_args(args, &root);

    if parsed.verify_only {
        verify_dist(&parsed.out);
        return;
    }

    if !parsed.skip_build {
        tracing::info!("🔨 编译官方插件（release）");
        crate::build_plugins(true, false, &mut None); // 编译 + 拷贝到 builtin/<id>/bin
    } else {
        tracing::info!("⏭  跳过编译（--skip-build）");
    }

    // 数据集：给了源数据就先构建（basic/ety 各一次），否则复用已有产物
    if !parsed.plugins_only {
        if let Some(csv) = parsed.ecdict.clone() {
            tracing::info!("📦 构建基础词库（ECDICT → SQLite）");
            crate::dictionary::run(&[
                "dictionary".into(),
                "--build-basic".into(),
                "--ecdict".into(),
                csv,
                "--out".into(),
                DATASET_BUILD_ROOT.into(),
            ]);
        }
        if let Some(wordroot) = parsed.wordroot.clone() {
            tracing::info!("📦 构建词源增强包");
            crate::dictionary::run(&[
                "dictionary".into(),
                "--build-ety".into(),
                "--wordroot".into(),
                wordroot,
                "--out".into(),
                DATASET_BUILD_ROOT.into(),
            ]);
        }
    }

    std::fs::create_dir_all(&parsed.out)
        .unwrap_or_else(|e| panic!("创建 {} 失败: {e}", parsed.out.display()));

    let platform = platform_tag();
    let mut catalog = serde_json::json!({
        "format_version": 1,
        "generated_at": crate::rfc3339_now_utc(),
    });
    let mut plugins = Vec::new();
    let mut datasets = Vec::new();
    let mut manifest_files: BTreeMap<String, (String, u64)> = BTreeMap::new(); // 文件名 → (sha256, bytes)

    // ── 插件 zip：plugins/builtin/<id>/ 全目录，manifest.json 在 zip 根 ──
    let builtin_dir = root.join("plugins").join("builtin");
    let mut plugin_dirs: Vec<PathBuf> = std::fs::read_dir(&builtin_dir)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {e}", builtin_dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("manifest.json").is_file())
        .collect();
    plugin_dirs.sort();
    if plugin_dirs.is_empty() {
        panic!("plugins/builtin/ 下没有可打包的插件（manifest.json 缺失）");
    }

    for dir in &plugin_dirs {
        let manifest: serde_json::Value = read_json(&dir.join("manifest.json"));
        let plugin_id = manifest["id"].as_str().unwrap_or_default().to_string();
        let version = manifest["version"].as_str().unwrap_or_default().to_string();
        if plugin_id.is_empty() || version.is_empty() {
            panic!("{} 的 manifest 缺少 id/version", dir.display());
        }
        let filename = format!("{plugin_id}-{version}-{platform}.zip");
        let zip_path = parsed.out.join(&filename);
        let file_count = zip_dir(dir, &zip_path);
        let (sha, bytes) = sha256_and_size(&zip_path);
        tracing::info!(
            "  ✓ {filename}（{file_count} 文件，{} MB）",
            bytes / 1024 / 1024
        );
        manifest_files.insert(filename.clone(), (sha.clone(), bytes));

        // 有 bin/ 的插件只能在构建平台运行；纯页面插件任意平台
        let platforms = if dir.join("bin").is_dir() {
            vec![platform.clone()]
        } else {
            vec![]
        };
        plugins.push(project_plugin_entry(
            &manifest,
            &version,
            &platforms,
            &filename,
            sha,
            bytes,
            &parsed.base,
        ));
    }

    // ── 数据集 zip：target/dictionary/<dataset_id>/<version>/，dataset.json 在根 ──
    if !parsed.plugins_only {
        let ds_root = root.join(DATASET_BUILD_ROOT);
        let mut ds_versions: Vec<(String, String)> = Vec::new(); // (dataset_id, version)
        if ds_root.is_dir() {
            for entry in std::fs::read_dir(&ds_root).unwrap().flatten() {
                let ds_dir = entry.path();
                if !ds_dir.is_dir() {
                    continue;
                }
                for v_entry in std::fs::read_dir(&ds_dir).unwrap().flatten() {
                    if v_entry.path().join("dataset.json").is_file() {
                        ds_versions.push((
                            entry.file_name().to_string_lossy().into_owned(),
                            v_entry.file_name().to_string_lossy().into_owned(),
                        ));
                    }
                }
            }
        }
        ds_versions.sort();
        if ds_versions.is_empty() {
            panic!(
                "未找到词库产物（{}）。先运行 `cargo xtask pack --ecdict <csv>` \
                 或 `cargo xtask dictionary --build-basic ...` 构建；\
                 跳过数据集用 --plugins-only",
                ds_root.display()
            );
        }
        for (dataset_id, version) in &ds_versions {
            let dir = ds_root.join(dataset_id).join(version);
            let ds_meta: serde_json::Value = read_json(&dir.join("dataset.json"));
            let plugin_id = ds_meta["plugin_id"].as_str().unwrap_or_default().to_string();
            if plugin_id.is_empty() {
                panic!("{} 的 dataset.json 缺少 plugin_id", dir.display());
            }
            let filename = format!("{dataset_id}-{version}.zip");
            let zip_path = parsed.out.join(&filename);
            let file_count = zip_dir(&dir, &zip_path);
            let (sha, bytes) = sha256_and_size(&zip_path);
            tracing::info!(
                "  ✓ {filename}（{file_count} 文件，{} MB）",
                bytes / 1024 / 1024
            );
            manifest_files.insert(filename.clone(), (sha.clone(), bytes));
            datasets.push(serde_json::json!({
                "plugin_id": plugin_id,
                "dataset_id": dataset_id,
                "version": version,
                "asset": {"url": format!("{}/{filename}", parsed.base), "sha256": sha, "bytes": bytes},
            }));
        }
    }

    catalog["plugins"] = serde_json::Value::Array(plugins);
    catalog["datasets"] = serde_json::Value::Array(datasets);
    let catalog_path = parsed.out.join(CATALOG_FILE);
    std::fs::write(&catalog_path, serde_json::to_string_pretty(&catalog).unwrap())
        .unwrap_or_else(|e| panic!("写 {} 失败: {e}", catalog_path.display()));
    tracing::info!("📝 {}（{} 插件 / {} 数据集）", catalog_path.display(),
        catalog["plugins"].as_array().map(Vec::len).unwrap_or(0),
        catalog["datasets"].as_array().map(Vec::len).unwrap_or(0));

    // 产物自校验（构建即验：上传前问题在本机暴露）
    verify_dist(&parsed.out);
    print_release_evidence(&manifest_files);
}

/// 打印发布证据（文件 → sha256/bytes），CI Release body 直接引用。
fn print_release_evidence(files: &BTreeMap<String, (String, u64)>) {
    tracing::info!("📋 发布证据（上传 CI Release 后生效）：");
    for (name, (sha, bytes)) in files {
        tracing::info!("  {name}  sha256={sha}  bytes={bytes}");
    }
}

/// 校验 dist 目录：catalog 引用的每个资产存在、哈希匹配、zip 根有清单文件。
fn verify_dist(out: &Path) {
    let catalog_path = out.join(CATALOG_FILE);
    let raw = std::fs::read_to_string(&catalog_path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {e}（先运行 pack）", catalog_path.display()));
    let catalog: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("catalog.json 非法: {e}"));

    let mut checked = 0;
    for entry in catalog["plugins"].as_array().unwrap_or(&vec![]).into_iter() {
        check_asset(out, entry, "manifest.json");
        checked += 1;
    }
    for entry in catalog["datasets"].as_array().unwrap_or(&vec![]).into_iter() {
        check_asset(out, entry, "dataset.json");
        checked += 1;
    }
    if checked == 0 {
        panic!("catalog 无条目，无事可校验");
    }
    tracing::info!("✅ 校验通过：{checked} 个资产完整且 zip 根清单存在");
}

fn check_asset(out: &Path, entry: &serde_json::Value, root_marker: &str) {
    let id = entry["plugin_id"].as_str().unwrap_or("?").to_string();
    let url = entry["asset"]["url"].as_str().unwrap_or("");
    let filename = url.rsplit('/').next().unwrap_or_default();
    let path = out.join(filename);
    let bytes = std::fs::metadata(&path)
        .unwrap_or_else(|e| panic!("资产缺失: {}（{e}）", path.display()))
        .len();
    assert_eq!(
        bytes,
        entry["asset"]["bytes"].as_u64().unwrap_or(0),
        "{id} 字节数与目录声明不符"
    );
    let sha = sha256_file_hex(&path);
    assert_eq!(
        sha,
        entry["asset"]["sha256"].as_str().unwrap_or(""),
        "{id} sha256 与目录声明不符"
    );
    // zip 根必须有清单文件（安装状态机从 zip 根解析）
    let file = std::fs::File::open(&path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap_or_else(|e| panic!("{id} zip 打开失败: {e}"));
    let marker_prefix = format!("{root_marker}/");
    let mut has_marker = false;
    for i in 0..zip.len() {
        let Ok(name) = zip.by_index(i).map(|f| f.name().map(|n| n.into_owned())) else {
            continue;
        };
        if let Ok(name) = name
            && (name == root_marker || name.starts_with(&marker_prefix))
        {
            has_marker = true;
            break;
        }
    }
    assert!(has_marker, "{id} zip 根缺少 {root_marker}");
}

/// 插件目录条目投影（0.28.12 展示元数据）：manifest 已有字段原样抄录——
/// name/description 双语、channel（缺省 stable）、has_pages 由 `pages` 推导。
/// updated_at 不填——本机 pack 没有可靠发布时间，不把文件修改时间当发布时间。
fn project_plugin_entry(
    manifest: &serde_json::Value,
    version: &str,
    platforms: &[String],
    filename: &str,
    sha: String,
    bytes: u64,
    base: &str,
) -> serde_json::Value {
    let host_min = manifest["host"]["min_version"]
        .as_str()
        .unwrap_or("0")
        .to_string();
    serde_json::json!({
        "plugin_id": manifest["id"],
        "version": version,
        "host_min_version": host_min,
        "platforms": platforms,
        "asset": {"url": format!("{base}/{filename}"), "sha256": sha, "bytes": bytes},
        "name": manifest["name"],
        "description": manifest["description"],
        "has_pages": manifest["pages"].as_array().is_some_and(|p| !p.is_empty()),
        "channel": manifest["channel"].as_str().unwrap_or("stable"),
    })
}

fn parse_args(args: &[String], root: &Path) -> PackArgs {    let value_of = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .filter(|v| !v.starts_with("--"))
    };
    let base = value_of("--base").unwrap_or_else(|| default_base_url(root));
    let out = value_of("--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target").join("plugin-dist"));
    PackArgs {
        base,
        out,
        skip_build: args.iter().any(|a| a == "--skip-build"),
        plugins_only: args.iter().any(|a| a == "--plugins-only"),
        ecdict: value_of("--ecdict"),
        wordroot: value_of("--wordroot"),
        verify_only: args.iter().any(|a| a == "--verify-only"),
    }
}

/// 默认资产基址：官方仓固定 tag `plugins-catalog` 的 Release（与应用端
/// catalog_endpoint 同一约定；发布工作流把全部资产上传到该 Release）。
fn default_base_url(root: &Path) -> String {
    let repo = crate::extract_toml_repository(&std::fs::read_to_string(root.join("Cargo.toml")).unwrap())
        .unwrap_or_else(|| panic!("根 Cargo.toml 缺少 repository，pack 需要 --base 显式指定资产基址"));
    format!("{repo}/releases/download/plugins-catalog")
}

fn platform_tag() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => other,
    };
    format!("{}-{arch}", std::env::consts::OS)
}

fn read_json(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{} 非法 JSON: {e}", path.display()))
}

/// 目录 → zip（递归；清单文件保持在 zip 根）。返回写入的文件数。
fn zip_dir(dir: &Path, dest: &Path) -> usize {
    let file = std::fs::File::create(dest)
        .unwrap_or_else(|e| panic!("创建 {} 失败: {e}", dest.display()));
    let mut zip = zip::ZipWriter::new(file);
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(cur) = stack.pop() {
        for entry in std::fs::read_dir(&cur).unwrap_or_else(|e| panic!("读取 {} 失败: {e}", cur.display())) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                paths.push(path);
            }
        }
    }
    paths.sort();
    for path in paths {
        let rel = path.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
        let content = std::fs::read(&path).unwrap();
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
        zip.start_file(&rel, opts)
            .unwrap_or_else(|e| panic!("zip 写入 {rel} 失败: {e}"));
        zip.write_all(&content).unwrap();
        count += 1;
    }
    zip.finish().unwrap();
    count
}

fn sha256_and_size(path: &Path) -> (String, u64) {
    let bytes = std::fs::metadata(path).unwrap().len();
    (sha256_file_hex(path), bytes)
}

fn sha256_file_hex(path: &Path) -> String {
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path).unwrap();
    std::io::copy(&mut file, &mut hasher).unwrap();
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.28.12：目录条目从 manifest 投影展示元数据（name/description/channel/
    /// has_pages），updated_at 不填（无可靠发布时间）。产物必须能被应用端
    /// `parse_catalog` 接受（xtask 与 domain 的目录 schema 双向对齐）。
    #[test]
    fn project_plugin_entry_carries_display_metadata() {
        let manifest: serde_json::Value = serde_json::from_str(
            r#"{"id":"builtin.dictionary","version":"0.2.0",
                "name":{"zh":"离线词典","en":"Dictionary"},
                "description":{"zh":"离线查词","en":"Offline lookup"},
                "pages":[{"id":"main","entry":"web/index.html"}],
                "host":{"min_version":"0.28.1"},
                "channel":"beta"}"#,
        )
        .unwrap();
        let entry = project_plugin_entry(
            &manifest,
            "0.2.0",
            &["windows-x86_64".to_string()],
            "builtin.dictionary-0.2.0-windows-x86_64.zip",
            "a".repeat(64),
            1234,
            "https://example.invalid/plugins-catalog",
        );
        assert_eq!(entry["name"]["zh"], "离线词典");
        assert_eq!(entry["description"]["en"], "Offline lookup");
        assert_eq!(entry["has_pages"], true);
        assert_eq!(entry["channel"], "beta");
        assert_eq!(entry["host_min_version"], "0.28.1");
        assert_eq!(
            entry["asset"]["url"],
            "https://example.invalid/plugins-catalog/builtin.dictionary-0.2.0-windows-x86_64.zip"
        );
        assert!(entry.get("updated_at").is_none());
        // 端到端校验（pack 产物形态）：zip 根清单等由 verify_dist 把关；此处
        // 确认目录文档完整可序列化。schema 与应用端 parse_catalog 的对齐由
        // blink-domain catalog 测试用同形 JSON 锁定，xtask 不引 domain 依赖。
        let catalog = serde_json::json!({
            "format_version": 1,
            "generated_at": "2026-10-28T00:00:00Z",
            "plugins": [entry],
            "datasets": [],
        });
        assert!(serde_json::to_string_pretty(&catalog).is_ok());
    }

    /// 无展示字段的极简 manifest 投影：name/description 投影为 null（应用端
    /// Option → None → 前端回退 plugin_id/无描述），has_pages=false，channel=stable。
    #[test]
    fn project_plugin_entry_minimal_manifest_falls_back() {
        let manifest: serde_json::Value = serde_json::from_str(
            r#"{"id":"extra.demo","version":"1.0.0"}"#,
        )
        .unwrap();
        let entry = project_plugin_entry(
            &manifest, "1.0.0", &[], "extra.demo-1.0.0.zip",
            "b".repeat(64), 10, "https://example.invalid/plugins-catalog",
        );
        assert!(entry["name"].is_null());
        assert!(entry["description"].is_null());
        assert_eq!(entry["has_pages"], false);
        assert_eq!(entry["channel"], "stable");
    }
}
