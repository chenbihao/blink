//! `cargo xtask funasr-worker` — 构建 Blink 常驻 GGUF STT worker（0.22.7）。
//!
//! 从锁定的 FunASR 源码（commit 55b662c == runtime-llamacpp-v0.2.6）出发，
//! 应用 Blink 的最小 stdin-server 补丁（NDJSON 协议，见
//! `xtask/funasr-worker/blink_worker_protocol.h`），用 CMake + Ninja + MSVC
//! 构建三个 worker，输出到 `resources/bin/funasr-worker/` 并生成 SHA-256
//! manifest。构建产物不入 Git（.gitignore），随 release 打包分发。
//!
//! 供应链锁定（运行期不跟随 main 漂移）：
//! - FunASR：commit `55b662ccf9ea77237ba9253b3bddd953d4184f84`
//!   （= 官方 release `runtime-llamacpp-v0.2.6`，MIT）
//! - llama.cpp：由 FunASR 的 CMakeLists FetchContent 锁定在
//!   `803b7fcae893e9caaee3921779628fef83ac0965`（MIT），构建期拉取
//! - GGUF 模型：由 Blink 引擎安装时从 HuggingFace 锁定 URL 下载（见
//!   Rust 侧 worker-lock.json / model catalog），本命令不下载模型
//!
//! 前置要求：VS 2022 BuildTools（含 C++ 工具链）+ Git；CMake/Ninja 优先取
//! PATH，缺失时回退 VS 自带版本。

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// 锁定的 FunASR 源码（= runtime-llamacpp-v0.2.6）。
pub(crate) const FUNASR_COMMIT: &str = "55b662ccf9ea77237ba9253b3bddd953d4184f84";
pub(crate) const FUNASR_RELEASE_TAG: &str = "runtime-llamacpp-v0.2.6";
pub(crate) const FUNASR_ZIP_URL: &str =
    "https://codeload.github.com/modelscope/FunASR/zip/55b662ccf9ea77237ba9253b3bddd953d4184f84";
pub(crate) const FUNASR_ZIP_SHA256: &str =
    "e60ba3843f1a3153c11830e3092f767d983ff6ebf3e1e4dcec7d13f3b45e5bf3";
const FUNASR_LICENSE: &str = "MIT (modelscope/FunASR)";
/// FunASR CMakeLists 锁定的 llama.cpp commit（构建期 FetchContent 拉取）。
pub(crate) const LLAMA_CPP_COMMIT: &str = "803b7fcae893e9caaee3921779628fef83ac0965";
const LLAMA_CPP_LICENSE: &str = "MIT (ggml-org/llama.cpp)";

/// 构建的 worker：cmake target → 发布文件名。
const WORKERS: &[(&str, &str)] = &[
    ("llama-funasr-sensevoice", "funasr-sensevoice-worker.exe"),
    ("llama-funasr-paraformer", "funasr-paraformer-worker.exe"),
    ("llama-funasr-cli", "funasr-nano-worker.exe"),
];

/// patch 相对 src 根的应用顺序（与仓库 xtask/funasr-worker/patches 对应）。
const PATCHES: &[&str] = &[
    "0001-sensevoice-ndjson-stdin-server.patch",
    "0002-paraformer-ndjson-stdin-server.patch",
    "0003-funasr-cli-ndjson-stdin-server.patch",
];

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let data = std::fs::read(path)?;
    hasher.update(&data);
    let out = hasher.finalize();
    Ok(out.iter().map(|b| format!("{b:02x}")).collect())
}

/// 运行命令，失败 panic（带上下文）。
fn run_ctx(cmd: &str, args: &[&str], cwd: &Path, desc: &str) {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("{desc}: 启动 {cmd} 失败: {e}"));
    if !status.success() {
        panic!("{desc}: {cmd} {} 失败 exit={status}", args.join(" "));
    }
}

/// 定位 VS 安装根（vcvars64.bat 所在 VS 的 installationPath）。
fn find_vs_install() -> Option<PathBuf> {
    // 优先 vswhere（VS 官方发现机制）
    let vswhere = PathBuf::from(
        std::env::var("ProgramFiles(x86)")
            .unwrap_or_else(|_| "C:\\Program Files (x86)".to_string()),
    )
    .join("Microsoft Visual Studio")
    .join("Installer")
    .join("vswhere.exe");
    if vswhere.exists() {
        if let Ok(out) = Command::new(&vswhere)
            .args([
                "-latest",
                "-products",
                "*",
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-property",
                "installationPath",
            ])
            .output()
        {
            if out.status.success() {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(PathBuf::from(path));
                }
            }
        }
    }
    // 回退：常见固定路径
    for base in [
        "C:\\Program Files\\Microsoft Visual Studio\\2022",
        "C:\\Program Files (x86)\\Microsoft Visual Studio\\2022",
    ] {
        for edition in ["BuildTools", "Community", "Professional", "Enterprise"] {
            let p = PathBuf::from(base).join(edition);
            if p.join("VC")
                .join("Auxiliary")
                .join("Build")
                .join("vcvars64.bat")
                .exists()
            {
                return Some(p);
            }
        }
    }
    None
}

/// 定位工具：优先 PATH，其次 VS 自带。
fn find_tool(name: &str, vs_fallback: Option<PathBuf>) -> PathBuf {
    if let Ok(found) = which_path(name) {
        return found;
    }
    if let Some(vs) = vs_fallback {
        let bundled = vs
            .join("Common7")
            .join("IDE")
            .join("CommonExtensions")
            .join("Microsoft")
            .join("CMake")
            .join(match name {
                "cmake" => "CMake\\bin\\cmake.exe",
                "ninja" => "Ninja\\ninja.exe",
                _ => name,
            });
        if bundled.exists() {
            return bundled;
        }
    }
    panic!("找不到 {name}。请安装 VS 2022 BuildTools（含 C++ 工具链）或把 {name} 加入 PATH");
}

fn which_path(name: &str) -> Result<PathBuf, ()> {
    let exts = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_string())
            .split(';')
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    } else {
        vec![String::new()]
    };
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            for ext in &exts {
                let cand = PathBuf::from(dir).join(format!("{name}{ext}"));
                if cand.is_file() {
                    return Ok(cand);
                }
            }
        }
    }
    Err(())
}

/// 确保锁定的 FunASR 源码已下载、校验、解压并应用补丁。
/// 幂等：`.blink-applied` 标记存在则跳过。
fn ensure_patched_source(work_root: &Path, patches_dir: &Path, header: &Path) -> PathBuf {
    let src_root = work_root.join("src");
    let llama_dir = src_root.join("runtime").join("llama.cpp");
    let marker = src_root.join(".blink-applied");

    if marker.exists() && llama_dir.join("CMakeLists.txt").exists() {
        // 头文件始终刷新——协议头是活跃开发面，marker 只跳过"下载+解压+补丁"
        // 这类重操作；不刷新会导致修改后的协议头静默不进构建。
        let common = llama_dir.join("funasr-common");
        if common.exists() {
            let _ = std::fs::copy(header, common.join("blink_worker_protocol.h"));
        }
        println!("📁 已存在补丁化的 FunASR 源码: {}", src_root.display());
        return llama_dir;
    }

    // 1. 下载 + 校验 zip
    let zip_path = work_root.join("funasr-source.zip");
    let need_download = !zip_path.exists()
        || sha256_file(&zip_path)
            .map(|h| h != FUNASR_ZIP_SHA256)
            .unwrap_or(true);
    if need_download {
        println!(
            "⬇️  下载 FunASR 源码 {FUNASR_RELEASE_TAG} ({}...)",
            &FUNASR_COMMIT[..12]
        );
        std::fs::create_dir_all(work_root).unwrap();
        run_ctx(
            "curl",
            &[
                "-sSL",
                "--fail",
                "-o",
                zip_path.to_str().unwrap(),
                FUNASR_ZIP_URL,
            ],
            work_root,
            "下载 FunASR 源码",
        );
    }
    let actual = sha256_file(&zip_path).unwrap_or_else(|e| panic!("读取 zip 失败: {e}"));
    assert_eq!(
        actual, FUNASR_ZIP_SHA256,
        "FunASR 源码 zip SHA-256 不匹配：期望 {FUNASR_ZIP_SHA256}，实际 {actual}"
    );
    println!("✅ 源码 zip SHA-256 校验通过");

    // 2. 解压（tar.exe 随 Windows 分发，支持 zip）
    if src_root.exists() {
        std::fs::remove_dir_all(&src_root).unwrap();
    }
    std::fs::create_dir_all(&src_root).unwrap();
    println!("📦 解压源码...");
    run_ctx(
        "tar",
        &[
            "-xf",
            zip_path.to_str().unwrap(),
            "-C",
            src_root.to_str().unwrap(),
        ],
        work_root,
        "解压 FunASR 源码",
    );
    // codeload zip 展开为 FunASR-<sha>/，归一到 src 根
    let mut inner: Option<PathBuf> = None;
    for entry in std::fs::read_dir(&src_root).unwrap().flatten() {
        if entry.path().is_dir() {
            inner = Some(entry.path());
        }
    }
    let inner = inner.expect("zip 应含单一顶层目录");
    for item in std::fs::read_dir(&inner).unwrap().flatten() {
        let dest = src_root.join(item.file_name());
        std::fs::rename(item.path(), &dest).unwrap();
    }
    std::fs::remove_dir(&inner).unwrap();

    // 3. 复制共享协议头到 funasr-common（编译期 include 路径已在 CMake 配好）
    let common_dir = llama_dir.join("funasr-common");
    std::fs::create_dir_all(&common_dir).unwrap();
    std::fs::copy(header, common_dir.join("blink_worker_protocol.h"))
        .expect("复制 blink_worker_protocol.h 失败");

    // 4. 应用补丁。
    //    在解压目录先 `git init + add`：`git apply` 在外层仓库（blink）内运行时，
    //    target/ 被 .gitignore 忽略的路径会被静默 "Skipped patch"（exit 0），
    //    独立仓库保证补丁真正落到文件上。
    run_ctx("git", &["init", "-q", "."], &src_root, "git init 解压目录");
    run_ctx("git", &["add", "-A"], &src_root, "git add 解压目录");
    for patch in PATCHES {
        let patch_path = patches_dir.join(patch);
        println!("🩹 应用补丁 {patch}...");
        let status = Command::new("git")
            .args(["apply", "--whitespace=nowarn"])
            .arg(&patch_path)
            .current_dir(&src_root)
            .status()
            .unwrap_or_else(|e| panic!("git apply 启动失败: {e}"));
        assert!(
            status.success(),
            "git apply {patch} 失败——源码 pin 与补丁漂移，请核对"
        );
    }

    std::fs::write(&marker, b"ok").unwrap();
    println!("✅ 源码补丁化完成: {}", src_root.display());
    llama_dir
}

/// 主入口。
pub fn build_workers() {
    let root = workspace_root();
    let work_root = root.join("target").join("funasr-worker");
    let patches_dir = root.join("xtask").join("funasr-worker").join("patches");
    let header = root
        .join("xtask")
        .join("funasr-worker")
        .join("blink_worker_protocol.h");
    let out_dir = root.join("resources").join("bin").join("funasr-worker");

    for p in [&patches_dir, &header] {
        assert!(p.exists(), "缺少 {}", p.display());
    }

    let llama_dir = ensure_patched_source(&work_root, &patches_dir, &header);

    // 工具链
    let vs = find_vs_install();
    let vcvars = vs
        .as_ref()
        .map(|v| {
            v.join("VC")
                .join("Auxiliary")
                .join("Build")
                .join("vcvars64.bat")
        })
        .filter(|p| p.exists());
    if vcvars.is_none() {
        panic!(
            "找不到 vcvars64.bat——请安装 Visual Studio 2022 BuildTools（含 C++ 桌面开发工作负载）"
        );
    }
    let vcvars = vcvars.unwrap();
    let cmake = find_tool("cmake", vs.clone());
    let ninja = find_tool("ninja", vs.clone());
    println!("🔧 cmake: {}", cmake.display());
    println!("🔧 ninja: {}", ninja.display());
    println!("🔧 vcvars: {}", vcvars.display());

    let build_dir = llama_dir.join("build-blink");

    // cmake configure（经 vcvars 环境执行，保证 cl.exe 可见）
    let configure = format!(
        // /utf-8：上游 funasr-cli 源码含 UTF-8 中文字面量（提示词），
        // MSVC 默认代码页（GBK）会报 C2001「常量中有换行符」。
        // /EHsc：blink_worker_protocol.h 使用 C++ 异常（inference_failed 归类），
        // 上游 CMake 未给这三个 target 设置异常展开语义。
        "call \"{}\" >nul && \"{}\" -B \"{}\" -G Ninja -DCMAKE_BUILD_TYPE=Release \
         -DCMAKE_CXX_FLAGS=\"/utf-8 /EHsc\" -DCMAKE_MAKE_PROGRAM=\"{}\" \"{}\"",
        vcvars.display(),
        cmake.display(),
        build_dir.display(),
        ninja.display(),
        llama_dir.display(),
    );
    println!("⚙️  CMake configure...");
    run_cmd_line(&configure, &llama_dir, "cmake configure");

    // build（3 个 target）
    let targets: Vec<&str> = WORKERS.iter().map(|(t, _)| *t).collect();
    let build_cmd = format!(
        "call \"{}\" >nul && \"{}\" --build \"{}\" --target {}",
        vcvars.display(),
        cmake.display(),
        build_dir.display(),
        targets.join(" "),
    );
    println!("🔨 构建 {} 个 worker target...", WORKERS.len());
    run_cmd_line(&build_cmd, &llama_dir, "cmake build");

    // 复制 + manifest
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut files = serde_json::Map::new();
    for (target, out_name) in WORKERS {
        let built = build_dir.join("bin").join(format!("{target}.exe"));
        assert!(built.exists(), "构建产物缺失: {}", built.display());
        let dest = out_dir.join(out_name);
        std::fs::copy(&built, &dest).unwrap();
        let hash = sha256_file(&dest).unwrap();
        let size = std::fs::metadata(&dest).unwrap().len();
        println!("✅ {} ({})", out_name, hash[..16].to_string());
        files.insert(
            out_name.to_string(),
            serde_json::json!({
                "sha256": hash,
                "size_bytes": size,
                "cmake_target": target,
            }),
        );
    }

    let manifest = serde_json::json!({
        "schema": 1,
        "protocol_version": 1,
        "funasr_release_tag": FUNASR_RELEASE_TAG,
        "funasr_commit": FUNASR_COMMIT,
        "funasr_source_zip_sha256": FUNASR_ZIP_SHA256,
        "llama_cpp_commit": LLAMA_CPP_COMMIT,
        "licenses": {
            "funasr": FUNASR_LICENSE,
            "llama_cpp": LLAMA_CPP_LICENSE,
            "blink_worker_protocol": "MIT (blink 仓库)",
        },
        "files": files,
        "built_at_unix_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    let manifest_path = out_dir.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    println!(
        "📦 worker 发布到 {}（manifest.json 含 SHA-256，安装期校验）",
        out_dir.display()
    );
}

/// 经 `cmd /d /s /c "<line>"` 执行多命令行（bash 不经手，避免引号转义损坏）。
///
/// 使用 `raw_arg` 绕过 Rust 的 Windows 参数转义——`cmd /s /c "..."` 语义
/// 要求整行作为一个带内嵌引号的参数原样传递。
fn run_cmd_line(line: &str, cwd: &Path, desc: &str) {
    use std::os::windows::process::CommandExt;
    let status = Command::new("cmd")
        .raw_arg(format!("/d /s /c \"{line}\""))
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("{desc}: 启动 cmd 失败: {e}"));
    assert!(status.success(), "{desc} 失败 exit={status}");
}
