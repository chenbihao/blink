//! `cargo xtask funasr-worker` — 构建 Blink FunASR runtime artifacts（0.22.16.3）。
//!
//! 从锁定的 FunASR 源码（commit 55b662c == runtime-llamacpp-v0.2.6）出发，
//! 应用 Blink 的最小 stdin-server 补丁（NDJSON 协议，见
//! `xtask/funasr-worker/blink_worker_protocol.h`），用 CMake + Ninja + MSVC
//! 构建三个 worker，输出基础 CPU artifact；Vulkan/CUDA 输出增量 backend
//! artifact，并生成 SHA-256 manifest。构建产物不入 Git（.gitignore）。
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

/// 锁定的 FunASR 源码（= runtime-llamacpp-v0.2.6）。
pub(crate) const FUNASR_COMMIT: &str = "55b662ccf9ea77237ba9253b3bddd953d4184f84";
pub(crate) const FUNASR_RELEASE_TAG: &str = "runtime-llamacpp-v0.2.6";
pub(crate) const FUNASR_ZIP_URL: &str =
    "https://codeload.github.com/modelscope/FunASR/zip/55b662ccf9ea77237ba9253b3bddd953d4184f84";
pub(crate) const FUNASR_ZIP_SHA256: &str =
    "e60ba3843f1a3153c11830e3092f767d983ff6ebf3e1e4dcec7d13f3b45e5bf3";
/// FunASR CMakeLists 锁定的 llama.cpp commit（构建期 FetchContent 拉取）。
pub(crate) const LLAMA_CPP_COMMIT: &str = "803b7fcae893e9caaee3921779628fef83ac0965";

/// Runtime artifact contract. Keep these identifiers in one place: the
/// profile/install work packages consume them and must not recreate strings.
pub(crate) const RUNTIME_VERSION: &str = "0.22.16.3";
pub(crate) const BASE_ARTIFACT_ID: &str = "funasr-runtime-windows-x64-base";
pub(crate) const VULKAN_ARTIFACT_ID: &str = "funasr-runtime-windows-x64-vulkan";
pub(crate) const CUDA_ARTIFACT_ID: &str = "funasr-runtime-windows-x64-cuda";
const ARTIFACT_MATRIX_REL: &str = "xtask/funasr-worker/artifact-matrix.json";
const RUNTIME_LOCK_REL: &str = "resources/stt/funasr-gguf/runtime-lock.json";
const ARTIFACT_MANIFEST_NAME: &str = "manifest.json";
const MANIFEST_SCHEMA: u64 = 2;
const DEFAULT_CUDA_ARCHS: &str = "75;86;89;90";
const PROTOCOL_HEADER_TARGET: &str = "runtime/llama.cpp/funasr-common/blink_worker_protocol.h";

const RUNTIME_ARTIFACT_CONTRACT: &[(&str, &[&str], bool)] = &[
    (BASE_ARTIFACT_ID, &[], true),
    (VULKAN_ARTIFACT_ID, &[BASE_ARTIFACT_ID], false),
    (CUDA_ARTIFACT_ID, &[BASE_ARTIFACT_ID], false),
];

/// 构建的 worker：cmake target → 发布文件名。
const WORKERS: &[(&str, &str)] = &[
    ("llama-funasr-sensevoice", "funasr-sensevoice-worker.exe"),
    ("llama-funasr-paraformer", "funasr-paraformer-worker.exe"),
    ("llama-funasr-cli", "funasr-nano-worker.exe"),
];

/// patch 应用顺序及其上游源码目标（与 xtask/funasr-worker/patches 对应）。
/// 目标路径用于定向暂存，避免 `git add -A` 扫描整个 FunASR 仓库。
const PATCH_INPUTS: &[(&str, &str)] = &[
    (
        "0001-sensevoice-ndjson-stdin-server.patch",
        "runtime/llama.cpp/sensevoice/funasr-sensevoice/funasr-sensevoice.cpp",
    ),
    (
        "0002-paraformer-ndjson-stdin-server.patch",
        "runtime/llama.cpp/paraformer/funasr-paraformer/funasr-paraformer.cpp",
    ),
    (
        "0003-funasr-cli-ndjson-stdin-server.patch",
        "runtime/llama.cpp/fun-asr-nano/funasr-cli/funasr-cli.cpp",
    ),
    (
        "0004-runtime-dynamic-backend-cmake.patch",
        "runtime/llama.cpp/CMakeLists.txt",
    ),
    (
        "0005-backend-adapter-public-graph-api.patch",
        "runtime/llama.cpp/funasr-common/blink_backend_adapter.h",
    ),
    (
        "0006-vad-uses-dynamic-cpu-backend-api.patch",
        "runtime/llama.cpp/funasr-common/funasr_vad.h",
    ),
    (
        "0007-cpu-backend-library-name.patch",
        "runtime/llama.cpp/funasr-common/blink_backend_adapter.h",
    ),
    (
        "0008-paraformer-vulkan-weights.patch",
        "runtime/llama.cpp/paraformer/funasr-paraformer/funasr-paraformer.cpp",
    ),
    (
        "0009-nano-cli-backend-call.patch",
        "runtime/llama.cpp/fun-asr-nano/funasr-cli/funasr-cli.cpp",
    ),
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

/// Backend DLL 名称判断。GPU backend 不能进入基础 CPU artifact。
fn is_gpu_dll(name: &str) -> bool {
    [
        "cuda", "cudart", "cublas", "vulkan", "hip", "metal", "opencl", "sycl",
    ]
    .iter()
    .any(|part| name.contains(part))
}

fn copy_runtime_dlls(build_bin: &Path, stage: &Path, flavor: BuildFlavor) -> Vec<String> {
    let mut copied = Vec::new();
    for entry in std::fs::read_dir(build_bin)
        .unwrap_or_else(|e| panic!("读取 CMake 输出目录失败（{}）: {e}", build_bin.display()))
        .flatten()
    {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if path.extension().is_none_or(|ext| ext != "dll") {
            continue;
        }
        let matches = match flavor {
            BuildFlavor::Base => {
                (lower.starts_with("ggml") || lower.starts_with("llama") || lower == "libomp.dll")
                    && !is_gpu_dll(&lower)
            }
            BuildFlavor::Vulkan => lower.starts_with("ggml-vulkan"),
            BuildFlavor::Cuda => lower.starts_with("ggml-cuda"),
        };
        if matches {
            copy_file(&path, &stage.join(name));
            copied.push(name.to_string());
        }
    }
    copied.sort();
    copied
}

fn copy_msvc_runtime(vcvars: &Path, stage: &Path) -> Vec<String> {
    let Some(vs_root) = vcvars
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
    else {
        panic!("无法从 vcvars64.bat 推导 Visual Studio 根目录");
    };
    let redist_root = vs_root.join("VC").join("Redist").join("MSVC");
    let mut candidates = Vec::new();
    if let Ok(versions) = std::fs::read_dir(&redist_root) {
        for version in versions.flatten() {
            let crt = version.path().join("x64").join("Microsoft.VC143.CRT");
            if crt.is_dir() {
                candidates.push(crt);
            }
        }
    }
    candidates.sort();
    let names = [
        "concrt140.dll",
        "msvcp140.dll",
        "msvcp140_1.dll",
        "msvcp140_2.dll",
        "msvcp140_codecvt_ids.dll",
        "vcruntime140.dll",
        "vcruntime140_1.dll",
    ];
    let mut copied = Vec::new();
    for name in names {
        let source = candidates
            .iter()
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("Visual C++ runtime 缺失: {name}"));
        copy_file(&source, &stage.join(name));
        copied.push(name.to_string());
    }
    copied
}

fn cuda_toolkit_bin() -> PathBuf {
    if let Ok(cuda_path) = std::env::var("CUDA_PATH") {
        let bin = PathBuf::from(cuda_path).join("bin");
        if bin.is_dir() {
            return bin;
        }
    }
    let nvcc = which_path("nvcc").expect("CUDA flavor 需要 nvcc");
    nvcc.parent()
        .map(Path::to_path_buf)
        .expect("无法确定 CUDA toolkit bin 目录")
}

fn copy_cuda_dependencies(stage: &Path) -> Vec<String> {
    let toolkit_bin = cuda_toolkit_bin();
    let mut copied = Vec::new();
    for entry in std::fs::read_dir(&toolkit_bin)
        .unwrap_or_else(|e| {
            panic!(
                "读取 CUDA toolkit bin 失败（{}）: {e}",
                toolkit_bin.display()
            )
        })
        .flatten()
    {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let required = lower.starts_with("cudart64_")
            || lower.starts_with("cublas64_")
            || lower.starts_with("cublaslt64_");
        if required && path.extension().is_some_and(|ext| ext == "dll") {
            copy_file(&path, &stage.join(name));
            copied.push(name.to_string());
        }
    }
    if copied.is_empty() {
        panic!(
            "CUDA backend 已构建但未找到 cudart/cublas 可再分发 DLL（目录 {}）",
            toolkit_bin.display()
        );
    }
    copied.sort();
    copied
}

fn copy_or_notice(destination: &Path, source: Option<&Path>, notice: &str) {
    if let Some(source) = source.filter(|path| path.is_file()) {
        copy_file(source, destination);
    } else {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(destination, notice)
            .unwrap_or_else(|e| panic!("写入依赖许可说明失败（{}）: {e}", destination.display()));
    }
}

fn add_licenses(
    root: &Path,
    source_root: &Path,
    build_dir: &Path,
    stage: &Path,
    flavor: BuildFlavor,
) {
    let license_dir = stage.join("licenses");
    std::fs::create_dir_all(&license_dir).unwrap();
    if flavor == BuildFlavor::Base {
        copy_or_notice(
            &license_dir.join("LICENSE-blink.txt"),
            Some(&root.join("LICENSE")),
            "Blink project license: MIT. See the repository LICENSE file.\n",
        );
        copy_or_notice(
            &license_dir.join("LICENSE-funasr.txt"),
            Some(&source_root.join("LICENSE")),
            "FunASR license: MIT (modelscope/FunASR).\nSource: https://github.com/modelscope/FunASR\n",
        );
        let llama_license = build_dir.join("_deps").join("llama-src").join("LICENSE");
        copy_or_notice(
            &license_dir.join("LICENSE-llama.cpp.txt"),
            Some(&llama_license),
            "llama.cpp license: MIT (ggml-org/llama.cpp).\nSource: https://github.com/ggml-org/llama.cpp\n",
        );
    }
    let notice = match flavor {
        BuildFlavor::Base => {
            "Runtime dependencies: Microsoft Visual C++ runtime as required by the host.\n"
        }
        BuildFlavor::Vulkan => {
            "Vulkan backend: MIT (ggml-org/llama.cpp). The Vulkan loader and GPU driver are supplied by the host driver installation; no vulkan-1.dll is bundled.\n"
        }
        BuildFlavor::Cuda => {
            "CUDA backend: MIT (ggml-org/llama.cpp). CUDA redistributable DLLs are copied from the explicitly selected CUDA Toolkit and remain subject to NVIDIA redistribution terms.\n"
        }
    };
    let notice_name = match flavor {
        BuildFlavor::Base => "THIRD-PARTY-NOTICES.txt",
        BuildFlavor::Vulkan => "THIRD-PARTY-NOTICES-vulkan.txt",
        BuildFlavor::Cuda => "THIRD-PARTY-NOTICES-cuda.txt",
    };
    std::fs::write(
        license_dir.join(notice_name),
        format!("{notice}FunASR pin: {FUNASR_COMMIT}\nllama.cpp pin: {LLAMA_CPP_COMMIT}\n"),
    )
    .unwrap();
    match flavor {
        BuildFlavor::Cuda => {
            let toolkit_root = cuda_toolkit_bin()
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let license = ["EULA.txt", "LICENSE.txt", "License.txt"]
                .iter()
                .map(|name| toolkit_root.join(name))
                .find(|path| path.is_file());
            copy_or_notice(
                &license_dir.join("LICENSE-cuda.txt"),
                license.as_deref(),
                "CUDA redistributable files remain subject to the NVIDIA CUDA Toolkit EULA; see the toolkit installation and https://docs.nvidia.com/cuda/eula/index.html.\n",
            );
        }
        BuildFlavor::Vulkan => {
            let license = std::env::var_os("VULKAN_SDK").and_then(|sdk| {
                ["LICENSE.txt", "License.txt", "README.txt"]
                    .iter()
                    .map(|name| PathBuf::from(&sdk).join(name))
                    .find(|path| path.is_file())
            });
            copy_or_notice(
                &license_dir.join("LICENSE-vulkan-sdk.txt"),
                license.as_deref(),
                "Vulkan SDK/loader licensing follows the installed Khronos/LunarG SDK terms; the host Vulkan loader is not bundled. See https://vulkan.lunarg.com/.\n",
            );
        }
        BuildFlavor::Base => {}
    }
}

fn probe_is_hardware_unavailable(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    [
        "hardware_unavailable",
        "device_unavailable",
        "no compatible device",
        "no device",
        "driver_unavailable",
        "driver not found",
        "vulkan device",
        "cuda device",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn run_backend_probe(exe: &Path, backend: &str, backend_dir: &Path) -> ProbeResult {
    let mut command = Command::new(exe);
    command
        .args([
            "--blink-backend-probe",
            "--backend",
            backend,
            "--backend-dir",
        ])
        .arg(backend_dir)
        .current_dir(backend_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command
        .output()
        .unwrap_or_else(|e| panic!("启动 backend probe 失败（{}）: {e}", exe.display()));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let mut text = stdout.clone();
    text.push_str(&stderr);
    let status = if output.status.success() && probe_output_is_valid(&stdout, backend) {
        "passed"
    } else if probe_is_hardware_unavailable(&text) {
        "pending"
    } else {
        "failed"
    };
    ProbeResult {
        status,
        exit_code: output.status.code(),
        output: text.chars().take(2000).collect(),
    }
}

fn probe_output_is_valid(output: &str, requested_backend: &str) -> bool {
    let Some(line) = output.lines().rev().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    value.get("type").and_then(|v| v.as_str()) == Some("backend_probe")
        && value.get("ok").and_then(|v| v.as_bool()) == Some(true)
        && value.get("requested_backend").and_then(|v| v.as_str()) == Some(requested_backend)
        && value.get("actual_backend").and_then(|v| v.as_str()) == Some(requested_backend)
        && value.pointer("/graph/status").and_then(|v| v.as_str()) == Some("success")
}

fn verify_exe_imports(vcvars: &Path, cwd: &Path, exe: &Path) -> String {
    let command = format!(
        "call \"{}\" >nul && dumpbin /DEPENDENTS \"{}\" 2>&1",
        vcvars.display(),
        exe.display()
    );
    let (status, output) = run_cmd_line_capture(&command, cwd);
    if !status {
        return "not-run-dumpbin-unavailable".to_string();
    }
    let lower = output.to_ascii_lowercase();
    for forbidden in [
        "ggml-vulkan.dll",
        "vulkan-1.dll",
        "ggml-cuda.dll",
        "cudart64_",
        "cublas64_",
    ] {
        assert!(
            !lower.contains(forbidden),
            "worker 静态 import 了 GPU runtime {forbidden}；GGML_BACKEND_DL=ON 仍必须保持 EXE 与 backend DLL 解耦（{}）",
            exe.display()
        );
    }
    "passed".to_string()
}

/// `.blink-applied` 不能只记录一个固定的 `ok`：否则 patch、协议头或源码 pin
/// 变化后，本地 release 会继续复用旧源码，而干净 CI 会重新应用新输入，导致
/// 两边构建内容分叉。指纹覆盖全部会改变补丁化源码的受控输入。
/// 计算补丁化源码缓存指纹。
fn patched_source_fingerprint(patches_dir: &Path, header: &Path) -> std::io::Result<String> {
    fn update_framed(hasher: &mut Sha256, label: &str, value: &[u8]) {
        hasher.update((label.len() as u64).to_le_bytes());
        hasher.update(label.as_bytes());
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value);
    }

    let mut hasher = Sha256::new();
    update_framed(&mut hasher, "schema", b"blink-funasr-patched-source-v1");
    update_framed(&mut hasher, "funasr_commit", FUNASR_COMMIT.as_bytes());
    update_framed(
        &mut hasher,
        "funasr_source_zip_sha256",
        FUNASR_ZIP_SHA256.as_bytes(),
    );
    update_framed(&mut hasher, "llama_cpp_commit", LLAMA_CPP_COMMIT.as_bytes());
    for (patch, _) in PATCH_INPUTS {
        update_framed(&mut hasher, "patch_name", patch.as_bytes());
        update_framed(
            &mut hasher,
            "patch_content",
            &std::fs::read(patches_dir.join(patch))?,
        );
    }
    update_framed(&mut hasher, "protocol_header", &std::fs::read(header)?);

    let digest = hasher.finalize();
    Ok(format!(
        "v1:{}\n",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
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
/// 幂等：`.blink-applied` 指纹与当前源码 pin、patch、协议头一致时跳过。
fn ensure_patched_source(work_root: &Path, patches_dir: &Path, header: &Path) -> PathBuf {
    let src_root = work_root.join("src");
    let llama_dir = src_root.join("runtime").join("llama.cpp");
    let marker = src_root.join(".blink-applied");
    let expected_marker = patched_source_fingerprint(patches_dir, header)
        .unwrap_or_else(|e| panic!("计算 FunASR 补丁源码指纹失败: {e}"));

    let marker_matches =
        std::fs::read_to_string(&marker).is_ok_and(|actual| actual == expected_marker);
    if marker_matches && llama_dir.join("CMakeLists.txt").exists() {
        println!("📁 已存在补丁化的 FunASR 源码: {}", src_root.display());
        return llama_dir;
    }
    if marker.exists() {
        println!("♻️  FunASR 构建输入已变化，重新展开并应用补丁");
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
    let copied_header = common_dir.join("blink_worker_protocol.h");
    std::fs::copy(header, &copied_header).expect("复制 blink_worker_protocol.h 失败");
    let header_text = std::fs::read_to_string(&copied_header)
        .unwrap_or_else(|e| panic!("读取 blink_worker_protocol.h 失败: {e}"));
    for marker in [
        "struct BackendMetadata",
        "--blink-backend-probe",
        "backend_json_fields",
        "type == \"health\"",
    ] {
        assert!(
            header_text.contains(marker),
            "blink_worker_protocol.h 缺少动态 backend 协议符号: {marker}"
        );
    }

    // 4. 应用补丁。
    //    在解压目录建立独立仓库：`git apply` 在外层仓库（blink）内运行时，
    //    target/ 被 .gitignore 忽略的路径会被静默 "Skipped patch"（exit 0）。
    //    临时仓库固定使用 LF，避免 Windows 全局 core.autocrlf 污染补丁上下文；
    //    只暂存三个补丁目标，避免扫描上游仓库中的无关文件和超长路径。
    run_ctx("git", &["init", "-q", "."], &src_root, "git init 解压目录");
    run_ctx(
        "git",
        &["config", "core.autocrlf", "false"],
        &src_root,
        "配置补丁仓库换行策略",
    );
    run_ctx(
        "git",
        &["config", "core.eol", "lf"],
        &src_root,
        "配置补丁仓库行尾",
    );
    // 0001 既新增 backend adapter，也会修改刚复制进去的共享协议头。
    // 先把协议头纳入临时仓库索引，git apply 才能把它当作已有上游文件
    // 做上下文补丁；不能只暂存三个 worker 的 cpp 文件。
    let mut add_args = vec!["add", "--", PROTOCOL_HEADER_TARGET];
    let existing_targets = PATCH_INPUTS
        .iter()
        .map(|(_, target)| *target)
        .filter(|target| src_root.join(target).is_file());
    add_args.extend(existing_targets);
    run_ctx("git", &add_args, &src_root, "git add 补丁目标");
    for (patch, _) in PATCH_INPUTS {
        let patch_path = patches_dir.join(patch);
        println!("🩹 应用补丁 {patch}...");
        let exclude_protocol = format!("--exclude={PROTOCOL_HEADER_TARGET}");
        let status = Command::new("git")
            // 共享协议头是仓库锁定输入，当前 patch 中还保留了其旧版本
            // 的重复 diff。先由上面的 marker 校验保证新协议头闭合，再
            // 只跳过该重复文件，让同一 patch 的 adapter/worker hunks 正常应用。
            .args(["apply", "--whitespace=nowarn"])
            .arg(&exclude_protocol)
            .arg(&patch_path)
            .current_dir(&src_root)
            .status()
            .unwrap_or_else(|e| panic!("git apply 启动失败: {e}"));
        assert!(
            status.success(),
            "git apply {patch} 失败——源码 pin 与补丁漂移，请核对"
        );
    }

    std::fs::write(&marker, expected_marker).unwrap();
    println!("✅ 源码补丁化完成: {}", src_root.display());
    llama_dir
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildFlavor {
    Base,
    Vulkan,
    Cuda,
}

impl BuildFlavor {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "base" | "cpu" => Some(Self::Base),
            "vulkan" => Some(Self::Vulkan),
            "cuda" => Some(Self::Cuda),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Vulkan => "vulkan",
            Self::Cuda => "cuda",
        }
    }

    fn backend(self) -> &'static str {
        match self {
            Self::Base => "cpu",
            Self::Vulkan => "vulkan",
            Self::Cuda => "cuda",
        }
    }

    fn artifact_id(self) -> &'static str {
        match self {
            Self::Base => BASE_ARTIFACT_ID,
            Self::Vulkan => VULKAN_ARTIFACT_ID,
            Self::Cuda => CUDA_ARTIFACT_ID,
        }
    }

    fn dependencies(self) -> &'static [&'static str] {
        match self {
            Self::Base => &[],
            Self::Vulkan | Self::Cuda => &[BASE_ARTIFACT_ID],
        }
    }

    fn packaging(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Vulkan | Self::Cuda => "incremental-backend",
        }
    }
}

#[derive(Debug)]
struct BuildOptions {
    flavors: Vec<BuildFlavor>,
    runtime_version: String,
    cuda_archs: String,
    require_hardware: bool,
}

impl BuildOptions {
    fn from_process_args() -> Self {
        let args: Vec<String> = std::env::args().skip(2).collect();
        let mut flavor_value = "base".to_string();
        let mut runtime_version = RUNTIME_VERSION.to_string();
        let mut cuda_archs =
            std::env::var("BLINK_CUDA_ARCHS").unwrap_or_else(|_| DEFAULT_CUDA_ARCHS.to_string());
        let mut require_hardware = false;
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            let value = |i: &mut usize, name: &str, args: &[String]| -> String {
                *i += 1;
                args.get(*i)
                    .cloned()
                    .unwrap_or_else(|| panic!("{name} 缺少值"))
            };
            if arg == "--flavor" {
                flavor_value = value(&mut i, "--flavor", &args);
            } else if let Some(v) = arg.strip_prefix("--flavor=") {
                flavor_value = v.to_string();
            } else if arg == "--version" {
                runtime_version = value(&mut i, "--version", &args);
            } else if let Some(v) = arg.strip_prefix("--version=") {
                runtime_version = v.to_string();
            } else if arg == "--cuda-archs" {
                cuda_archs = value(&mut i, "--cuda-archs", &args);
            } else if let Some(v) = arg.strip_prefix("--cuda-archs=") {
                cuda_archs = v.to_string();
            } else if arg == "--require-hardware" {
                require_hardware = true;
            } else if arg == "--help" || arg == "-h" {
                println!(
                    "用法: cargo xtask funasr-worker [--flavor base|vulkan|cuda|all] [--version X] [--cuda-archs 75;86] [--require-hardware]"
                );
                std::process::exit(0);
            } else {
                panic!("未知 funasr-worker 参数: {arg}");
            }
            i += 1;
        }
        let flavors = if flavor_value.eq_ignore_ascii_case("all") {
            vec![BuildFlavor::Base, BuildFlavor::Vulkan, BuildFlavor::Cuda]
        } else {
            vec![
                BuildFlavor::parse(&flavor_value)
                    .unwrap_or_else(|| panic!("不支持的 build flavor: {flavor_value}")),
            ]
        };
        assert_eq!(
            runtime_version, RUNTIME_VERSION,
            "runtime version 必须与 artifact-matrix.json 一致"
        );
        assert!(!cuda_archs.trim().is_empty(), "CUDA 架构列表不能为空");
        Self {
            flavors,
            runtime_version,
            cuda_archs,
            require_hardware,
        }
    }
}

#[derive(Debug)]
struct ProbeResult {
    status: &'static str,
    exit_code: Option<i32>,
    output: String,
}

fn read_json(path: &Path, desc: &str) -> serde_json::Value {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{desc}读取失败（{}）: {e}", path.display()));
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("{desc}不是合法 JSON: {e}"))
}

fn validate_artifact_matrix(root: &Path) {
    let matrix = read_json(&root.join(ARTIFACT_MATRIX_REL), "FunASR artifact matrix ");
    assert_eq!(
        matrix.get("runtime_version").and_then(|v| v.as_str()),
        Some(RUNTIME_VERSION),
        "artifact-matrix.json 的 runtime_version 漂移"
    );
    let artifacts = matrix
        .get("artifacts")
        .and_then(|v| v.as_array())
        .expect("artifact-matrix.json 缺少 artifacts 数组");
    for flavor in [BuildFlavor::Base, BuildFlavor::Vulkan, BuildFlavor::Cuda] {
        let item = artifacts
            .iter()
            .find(|item| {
                item.get("id").and_then(|v| v.as_str()) == Some(flavor.artifact_id())
                    && item.get("flavor").and_then(|v| v.as_str()) == Some(flavor.as_str())
            })
            .unwrap_or_else(|| {
                panic!(
                    "artifact-matrix.json 缺少 {} ({})",
                    flavor.as_str(),
                    flavor.artifact_id()
                )
            });
        let dependencies = item
            .get("depends_on")
            .and_then(|v| v.as_array())
            .map(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().unwrap_or_default())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                panic!(
                    "artifact-matrix.json 缺少 {} 的 depends_on",
                    flavor.as_str()
                )
            });
        assert_eq!(
            dependencies,
            flavor.dependencies(),
            "artifact matrix 依赖漂移"
        );
        assert_eq!(
            item.get("packaging").and_then(|v| v.as_str()),
            Some(flavor.packaging()),
            "artifact matrix 打包模型漂移"
        );
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn validate_runtime_lock_value(lock: &serde_json::Value) -> Vec<String> {
    let mut failures = Vec::new();
    if lock.get("schema").and_then(|v| v.as_u64()) != Some(1) {
        failures.push("runtime-lock.json schema 必须为 1".to_string());
    }
    if lock.get("runtime_version").and_then(|v| v.as_str()) != Some(RUNTIME_VERSION) {
        failures.push(format!(
            "runtime-lock.json runtime_version 必须为 {RUNTIME_VERSION}"
        ));
    }
    let expected_release_tag = format!("funasr-runtime-v{RUNTIME_VERSION}");
    if lock.get("release_tag").and_then(|v| v.as_str()) != Some(expected_release_tag.as_str()) {
        failures.push(format!(
            "runtime-lock.json release_tag 必须为 {expected_release_tag}"
        ));
    }
    if lock.get("platform").and_then(|v| v.as_str()) != Some("windows") {
        failures.push("runtime-lock.json platform 必须为 windows".to_string());
    }
    if lock.get("architecture").and_then(|v| v.as_str()) != Some("x86_64") {
        failures.push("runtime-lock.json architecture 必须为 x86_64".to_string());
    }

    let Some(artifacts) = lock.get("artifacts").and_then(|v| v.as_object()) else {
        failures.push("runtime-lock.json 缺少 artifacts 对象".to_string());
        return failures;
    };

    for (artifact_id, dependencies, required) in RUNTIME_ARTIFACT_CONTRACT {
        let Some(artifact) = artifacts.get(*artifact_id) else {
            failures.push(format!("runtime-lock.json 缺少 artifact: {artifact_id}"));
            continue;
        };
        let expected_asset = format!("{artifact_id}-v{RUNTIME_VERSION}.zip");
        if artifact.get("asset").and_then(|v| v.as_str()) != Some(expected_asset.as_str()) {
            failures.push(format!(
                "runtime artifact {artifact_id} asset 必须为 {expected_asset}"
            ));
        }
        let expected_url_suffix = format!("/download/{expected_release_tag}/{expected_asset}");
        match artifact.get("url").and_then(|v| v.as_str()) {
            Some(url) if url.starts_with("https://") && url.ends_with(&expected_url_suffix) => {}
            Some(url) => failures.push(format!(
                "runtime artifact {artifact_id} URL 必须是固定 release download URL，实际 {url}"
            )),
            None => failures.push(format!("runtime artifact {artifact_id} 缺少 url")),
        }
        match artifact.get("sha256").and_then(|v| v.as_str()) {
            Some(hash) if is_sha256(hash) => {}
            Some(hash) => failures.push(format!(
                "runtime artifact {artifact_id} SHA-256 无效（需要 64 位 hex，实际长度 {}）",
                hash.len()
            )),
            None => failures.push(format!(
                "runtime artifact {artifact_id} 尚未写入 SHA-256；禁止进入应用 release"
            )),
        }
        let actual_dependencies =
            artifact
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str())
                        .collect::<Vec<_>>()
                });
        if actual_dependencies.as_deref() != Some(*dependencies) {
            failures.push(format!(
                "runtime artifact {artifact_id} depends_on 漂移：期望 {dependencies:?}，实际 {actual_dependencies:?}"
            ));
        }
        if artifact.get("required").and_then(|v| v.as_bool()) != Some(*required) {
            failures.push(format!(
                "runtime artifact {artifact_id} required 标记漂移：期望 {required}"
            ));
        }
    }

    for artifact_id in artifacts.keys() {
        if !RUNTIME_ARTIFACT_CONTRACT
            .iter()
            .any(|(expected, _, _)| expected == artifact_id)
        {
            failures.push(format!(
                "runtime-lock.json 包含未声明 artifact: {artifact_id}"
            ));
        }
    }

    failures
}

/// 校验应用消费的 runtime lock；只读，不下载、不修改锁文件。
pub(crate) fn validate_runtime_lock(failures: &mut Vec<String>) {
    let root = workspace_root();
    let path = root.join(RUNTIME_LOCK_REL);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            failures.push(format!(
                "FunASR runtime lock 读取失败（{}）: {error}",
                path.display()
            ));
            return;
        }
    };
    let lock = match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(lock) => lock,
        Err(error) => {
            failures.push(format!(
                "FunASR runtime lock JSON 无效（{}）: {error}",
                path.display()
            ));
            return;
        }
    };
    failures.extend(validate_runtime_lock_value(&lock));
}

fn tool_version(path: &Path, args: &[&str]) -> String {
    let output = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("读取工具版本失败（{}）: {e}", path.display()));
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text.lines().next().unwrap_or("unknown").trim().to_string()
}

fn compiler_version(vcvars: &Path, cwd: &Path) -> String {
    let command = format!("call \"{}\" >nul && cl 2>&1", vcvars.display());
    let (_, output) = run_cmd_line_capture(&command, cwd);
    output
        .lines()
        .find(|line| line.contains("Version") || line.contains("版本"))
        .unwrap_or("unknown MSVC")
        .trim()
        .to_string()
}

fn toolkit_version(flavor: BuildFlavor) -> String {
    match flavor {
        BuildFlavor::Base => "not-used".to_string(),
        BuildFlavor::Vulkan => {
            let glslc = which_path("glslc").unwrap_or_else(|_| panic!("Vulkan flavor 需要 glslc"));
            format!("glslc {}", tool_version(&glslc, &["--version"]))
        }
        BuildFlavor::Cuda => {
            let nvcc = which_path("nvcc").unwrap_or_else(|_| panic!("CUDA flavor 需要 nvcc"));
            let output = Command::new(nvcc)
                .arg("--version")
                .output()
                .unwrap_or_else(|e| panic!("读取 CUDA toolkit 版本失败: {e}"));
            let mut text = String::from_utf8_lossy(&output.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text.lines()
                .find(|line| line.contains("release") || line.contains("V"))
                .unwrap_or("unknown CUDA toolkit")
                .trim()
                .to_string()
        }
    }
}

fn cmake_flags(flavor: BuildFlavor, cuda_archs: &str, ninja: &Path) -> Vec<String> {
    let mut flags = vec![
        "-DCMAKE_BUILD_TYPE=Release".to_string(),
        // GGML_BACKEND_DL=ON requires the ggml/llama shared libraries.  The
        // base artifact therefore carries the CPU DLLs, while Vulkan/CUDA
        // artifacts add only their backend DLLs and runtime dependencies.
        "-DBUILD_SHARED_LIBS=ON".to_string(),
        "-DGGML_BACKEND_DL=ON".to_string(),
        "-DGGML_CPU=ON".to_string(),
        "-DGGML_CPU_ALL_VARIANTS=ON".to_string(),
        "-DGGML_NATIVE=OFF".to_string(),
        "-DLLAMA_BUILD_TESTS=OFF".to_string(),
        "-DLLAMA_BUILD_EXAMPLES=OFF".to_string(),
        "-DLLAMA_BUILD_TOOLS=OFF".to_string(),
        "-DLLAMA_BUILD_SERVER=OFF".to_string(),
        "-DLLAMA_CURL=OFF".to_string(),
        "-DCMAKE_CXX_FLAGS=/utf-8 /EHsc".to_string(),
        format!("-DCMAKE_MAKE_PROGRAM={}", ninja.display()),
    ];
    match flavor {
        BuildFlavor::Base => {
            flags.push("-DGGML_VULKAN=OFF".to_string());
            flags.push("-DGGML_CUDA=OFF".to_string());
        }
        BuildFlavor::Vulkan => {
            flags.push("-DGGML_VULKAN=ON".to_string());
            flags.push("-DGGML_CUDA=OFF".to_string());
        }
        BuildFlavor::Cuda => {
            flags.push("-DGGML_VULKAN=OFF".to_string());
            flags.push("-DGGML_CUDA=ON".to_string());
            flags.push("-DGGML_CUDA_FORCE_CUBLAS=ON".to_string());
            flags.push(format!("-DCMAKE_CUDA_ARCHITECTURES={cuda_archs}"));
            flags.push(format!("-DGGML_CUDA_ARCHITECTURES={cuda_archs}"));
            flags.push("-DCMAKE_CUDA_RUNTIME_LIBRARY=Shared".to_string());
        }
    }
    flags
}

fn format_cmake_flag(flag: &str) -> String {
    if let Some((key, value)) = flag.split_once('=')
        && value.contains(' ')
    {
        format!("{key}=\"{value}\"")
    } else {
        flag.to_string()
    }
}

fn framed_hash(parts: &[(&str, String)]) -> String {
    let mut hasher = Sha256::new();
    for (label, value) in parts {
        hasher.update((label.len() as u64).to_le_bytes());
        hasher.update(label.as_bytes());
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[allow(clippy::too_many_arguments)]
fn build_fingerprint(
    source_fingerprint: &str,
    flavor: BuildFlavor,
    runtime_version: &str,
    cuda_archs: &str,
    flags: &[String],
    cmake_version: &str,
    ninja_version: &str,
    compiler_version: &str,
    toolkit_version: &str,
) -> String {
    let mut parts = vec![
        ("schema", "blink-funasr-runtime-build-v2".to_string()),
        ("runtime_version", runtime_version.to_string()),
        ("flavor", flavor.as_str().to_string()),
        ("cuda_architectures", cuda_archs.to_string()),
        ("source_fingerprint", source_fingerprint.to_string()),
        ("cmake_version", cmake_version.to_string()),
        ("ninja_version", ninja_version.to_string()),
        ("compiler_version", compiler_version.to_string()),
        ("toolkit_version", toolkit_version.to_string()),
    ];
    for flag in flags {
        parts.push(("cmake_flag", flag.clone()));
    }
    format!("v2:{}", framed_hash(&parts))
}

fn clear_dir(path: &Path) {
    if path.exists() {
        std::fs::remove_dir_all(path)
            .unwrap_or_else(|e| panic!("清理生成目录失败（{}）: {e}", path.display()));
    }
    std::fs::create_dir_all(path)
        .unwrap_or_else(|e| panic!("创建生成目录失败（{}）: {e}", path.display()));
}

fn copy_file(source: &Path, destination: &Path) {
    assert!(source.is_file(), "缺少构建文件: {}", source.display());
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::copy(source, destination).unwrap_or_else(|e| {
        panic!(
            "复制构建文件失败（{} -> {}）: {e}",
            source.display(),
            destination.display()
        )
    });
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("读取生成目录失败（{}）: {e}", dir.display()))
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or_else(|_| panic!("生成文件越出 artifact 根目录: {}", path.display()))
        .to_string_lossy()
        .replace('\\', "/")
}

fn copy_tree(source: &Path, destination: &Path) {
    for path in collect_files(source) {
        let relative = path.strip_prefix(source).unwrap();
        copy_file(&path, &destination.join(relative));
    }
}

fn package_artifact(stage: &Path, package_root: &Path) -> PathBuf {
    std::fs::create_dir_all(package_root).unwrap();
    let artifact_id = stage.file_name().and_then(|n| n.to_str()).unwrap();
    let zip = package_root.join(format!("{artifact_id}-v{RUNTIME_VERSION}.zip"));
    if zip.exists() {
        std::fs::remove_file(&zip).unwrap();
    }
    let command = format!(
        "tar -a -c -f \"{}\" -C \"{}\" .",
        zip.display(),
        stage.display()
    );
    run_cmd_line(&command, package_root, "打包 runtime artifact");
    assert!(zip.is_file(), "artifact zip 未生成: {}", zip.display());
    zip
}

fn write_checksum_file(package_root: &Path) {
    let mut packages: Vec<PathBuf> = std::fs::read_dir(package_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "zip"))
        .collect();
    packages.sort();
    let mut lines = Vec::new();
    for package in packages {
        let hash = sha256_file(&package).unwrap();
        lines.push(format!(
            "{hash}  {}",
            package.file_name().unwrap().to_string_lossy()
        ));
    }
    std::fs::write(package_root.join("SHA256SUMS.txt"), lines.join("\n") + "\n").unwrap();
}

/// 新的正式入口：显式 flavor/matrix、动态 backend、hash 完整性和可复现指纹。
pub fn build_workers() {
    let root = workspace_root();
    validate_artifact_matrix(&root);
    let options = BuildOptions::from_process_args();
    let work_root = root.join("target").join("funasr-worker");
    let patches_dir = root.join("xtask").join("funasr-worker").join("patches");
    let header = root
        .join("xtask")
        .join("funasr-worker")
        .join("blink_worker_protocol.h");
    for p in [&patches_dir, &header] {
        assert!(p.exists(), "缺少 {}", p.display());
    }
    let source_fingerprint = patched_source_fingerprint(&patches_dir, &header)
        .unwrap_or_else(|e| panic!("计算 FunASR 源码指纹失败: {e}"));
    let llama_dir = ensure_patched_source(&work_root, &patches_dir, &header);

    let vs = find_vs_install();
    let vcvars = vs
        .as_ref()
        .map(|v| {
            v.join("VC")
                .join("Auxiliary")
                .join("Build")
                .join("vcvars64.bat")
        })
        .filter(|p| p.exists())
        .unwrap_or_else(|| panic!("找不到 vcvars64.bat——请安装 VS 2022 BuildTools C++ 工作负载"));
    let cmake = find_tool("cmake", vs.clone());
    let ninja = find_tool("ninja", vs.clone());
    let package_root = root.join("target").join("funasr-runtime");
    std::fs::create_dir_all(&package_root).unwrap();

    for flavor in &options.flavors {
        let flags = cmake_flags(*flavor, &options.cuda_archs, &ninja);
        let cmake_version = tool_version(&cmake, &["--version"]);
        let ninja_version = tool_version(&ninja, &["--version"]);
        let compiler = compiler_version(&vcvars, &llama_dir);
        let toolkit = toolkit_version(*flavor);
        let fingerprint = build_fingerprint(
            &source_fingerprint,
            *flavor,
            &options.runtime_version,
            &options.cuda_archs,
            &flags,
            &cmake_version,
            &ninja_version,
            &compiler,
            &toolkit,
        );
        let short: String = fingerprint.chars().skip(3).take(16).collect();
        // Vulkan 的 shader generator 会在 build tree 下再嵌套 ExternalProject、
        // CMakeScratch 与 TryCompile。若把 build tree 放在 FunASR 深层源码目录，
        // Windows/MSVC 很容易超过 object/PDB 的路径上限并触发 C1041。
        // 构建目录保持在 workspace target 下的短路径；fingerprint 仍负责隔离缓存。
        let build_dir = root
            .join("target")
            .join("fw-build")
            .join(format!("{}-{short}", flavor.as_str()));
        let build_bin = build_dir.join("bin");
        let stage = package_root.join(flavor.artifact_id());
        clear_dir(&stage);

        let cmake_flag_string = flags
            .iter()
            .map(String::as_str)
            .map(format_cmake_flag)
            .collect::<Vec<_>>()
            .join(" ");
        let configure = format!(
            "call \"{}\" >nul && \"{}\" -B \"{}\" -G Ninja {} \"{}\"",
            vcvars.display(),
            cmake.display(),
            build_dir.display(),
            cmake_flag_string,
            llama_dir.display(),
        );
        println!("⚙️  CMake configure ({})...", flavor.as_str());
        run_cmd_line(&configure, &llama_dir, "cmake configure");

        let targets: Vec<&str> = WORKERS.iter().map(|(target, _)| *target).collect();
        let build_cmd = format!(
            "call \"{}\" >nul && \"{}\" --build \"{}\" --target {}",
            vcvars.display(),
            cmake.display(),
            build_dir.display(),
            targets.join(" "),
        );
        println!(
            "🔨 构建 {} flavor 的 {} 个 worker target...",
            flavor.as_str(),
            WORKERS.len()
        );
        run_cmd_line(&build_cmd, &llama_dir, "cmake build");

        if *flavor == BuildFlavor::Base {
            for (target, output_name) in WORKERS {
                copy_file(
                    &build_bin.join(format!("{target}.exe")),
                    &stage.join(output_name),
                );
            }
        }
        // GPU artifacts are incremental overlays. The install transaction
        // first commits the base artifact, then places this backend bundle in
        // the same deployment root; no runtime file is guessed at install.
        let backend_files = copy_runtime_dlls(&build_bin, &stage, *flavor);
        assert!(
            !backend_files.is_empty(),
            "{} artifact 缺少动态 {} backend DLL；确认 GGML_BACKEND_DL=ON",
            flavor.as_str(),
            flavor.backend()
        );
        if *flavor == BuildFlavor::Base {
            copy_msvc_runtime(&vcvars, &stage);
        }
        if *flavor == BuildFlavor::Cuda {
            copy_cuda_dependencies(&stage);
        }
        add_licenses(&root, &work_root.join("src"), &build_dir, &stage, *flavor);

        let probe_exe = if *flavor == BuildFlavor::Base {
            stage.join(WORKERS[0].1)
        } else {
            build_bin.join(format!("{}.exe", WORKERS[0].0))
        };
        let probe = run_backend_probe(
            &probe_exe,
            flavor.backend(),
            if *flavor == BuildFlavor::Base {
                &stage
            } else {
                &build_bin
            },
        );
        if probe.status == "failed" || (options.require_hardware && probe.status == "pending") {
            panic!(
                "{} backend probe 未通过（status={}, exit={:?}）:\n{}",
                flavor.as_str(),
                probe.status,
                probe.exit_code,
                probe.output
            );
        }
        let import_check = verify_exe_imports(&vcvars, &llama_dir, &probe_exe);
        let mut toolchain = BTreeMap::new();
        toolchain.insert("cmake_version".to_string(), cmake_version);
        toolchain.insert("ninja_version".to_string(), ninja_version);
        toolchain.insert("compiler_version".to_string(), compiler);
        toolchain.insert("toolkit_version".to_string(), toolkit);
        write_manifest(
            &stage,
            *flavor,
            &options.runtime_version,
            &fingerprint,
            &flags,
            &toolchain,
            &options.cuda_archs,
            &probe,
            &import_check,
        );
        validate_artifact_dir(&stage, Some(flavor.artifact_id()))
            .unwrap_or_else(|e| panic!("{} artifact 校验失败: {e}", flavor.as_str()));
        let zip = package_artifact(&stage, &package_root);
        println!("📦 {} artifact: {}", flavor.artifact_id(), zip.display());
    }

    let base_stage = package_root.join(BASE_ARTIFACT_ID);
    if options.flavors.contains(&BuildFlavor::Base) {
        let out_dir = root.join("resources").join("bin").join("funasr-worker");
        clear_dir(&out_dir);
        copy_tree(&base_stage, &out_dir);
        validate_artifact_dir(&out_dir, Some(BASE_ARTIFACT_ID))
            .unwrap_or_else(|e| panic!("bundled CPU runtime 校验失败: {e}"));
        println!("📦 基础 runtime 已同步到 {}", out_dir.display());
    }
    write_checksum_file(&package_root);
}

/// release-check 的只读文件闭包校验，不会因为查询而启动进程或下载文件。
pub(crate) fn validate_bundled_runtime(failures: &mut Vec<String>) {
    let root = workspace_root();
    let path = root.join("resources/bin/funasr-worker");
    if !path.is_dir() || !path.join(ARTIFACT_MANIFEST_NAME).is_file() {
        failures.push(format!("构建产物缺失（{}）", path.display()));
        return;
    }
    if let Err(error) = validate_artifact_dir(&path, Some(BASE_ARTIFACT_ID)) {
        failures.push(format!("FunASR runtime manifest/文件闭包校验失败: {error}"));
    }
}

/// 应用 release 的唯一 runtime 获取入口：固定 release tag、固定 URL、固定 SHA。
pub(crate) fn fetch_fixed_runtime() {
    let root = workspace_root();
    let lock = read_json(&root.join(RUNTIME_LOCK_REL), "FunASR runtime lock ");
    assert_eq!(
        lock.get("runtime_version").and_then(|v| v.as_str()),
        Some(RUNTIME_VERSION),
        "runtime-lock.json 版本漂移"
    );
    let artifact = lock
        .pointer(&format!("/artifacts/{BASE_ARTIFACT_ID}"))
        .expect("runtime-lock.json 缺少基础 artifact");
    let url = artifact
        .get("url")
        .and_then(|v| v.as_str())
        .filter(|value| !value.contains("latest") && value.contains("/download/"))
        .expect("runtime-lock.json 基础 artifact URL 必须是固定 release download URL");
    let expected_hash = artifact
        .get("sha256")
        .and_then(|v| v.as_str())
        .filter(|value| value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| {
            panic!("runtime-lock.json 尚未写入基础 artifact SHA-256；禁止用未锁定 runtime 打应用包")
        });
    let out_dir = root.join("resources/bin/funasr-worker");
    if out_dir.is_dir() && validate_artifact_dir(&out_dir, Some(BASE_ARTIFACT_ID)).is_ok() {
        println!("✅ 已存在经 manifest 校验的固定 FunASR runtime，跳过下载");
        return;
    }
    let download_root = root.join("target").join("funasr-runtime-download");
    clear_dir(&download_root);
    let asset = artifact
        .get("asset")
        .and_then(|v| v.as_str())
        .expect("runtime-lock.json 缺少基础 artifact asset");
    assert!(
        Path::new(asset).file_name().and_then(|n| n.to_str()) == Some(asset),
        "runtime asset 名称不安全: {asset}"
    );
    let archive = download_root.join(asset);
    run_ctx(
        "curl",
        &["-sSL", "--fail", "-o", archive.to_str().unwrap(), url],
        &download_root,
        "下载固定 FunASR runtime",
    );
    let actual_hash = sha256_file(&archive).unwrap();
    assert_eq!(actual_hash, expected_hash, "FunASR runtime SHA-256 不匹配");
    let extracted = download_root.join("extracted");
    std::fs::create_dir_all(&extracted).unwrap();
    run_ctx(
        "tar",
        &[
            "-xf",
            archive.to_str().unwrap(),
            "-C",
            extracted.to_str().unwrap(),
        ],
        &download_root,
        "解压固定 FunASR runtime",
    );
    validate_artifact_dir(&extracted, Some(BASE_ARTIFACT_ID))
        .unwrap_or_else(|e| panic!("下载的 FunASR runtime 文件闭包不可信: {e}"));
    clear_dir(&out_dir);
    copy_tree(&extracted, &out_dir);
    validate_artifact_dir(&out_dir, Some(BASE_ARTIFACT_ID))
        .unwrap_or_else(|e| panic!("同步 bundled runtime 失败: {e}"));
}

fn manifest_file_entries(stage: &Path) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    for path in collect_files(stage) {
        if path.file_name().and_then(|n| n.to_str()) == Some(ARTIFACT_MANIFEST_NAME) {
            continue;
        }
        let relative = relative_path(stage, &path);
        let hash = sha256_file(&path).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        let kind = if relative.starts_with("licenses/") {
            "license"
        } else if relative.ends_with(".exe") {
            "worker"
        } else if relative.ends_with(".dll") {
            "runtime"
        } else {
            "support"
        };
        entries.push(serde_json::json!({
            "path": relative,
            "kind": kind,
            "size_bytes": size,
            "sha256": hash,
        }));
    }
    entries.sort_by(|a, b| {
        a.get("path")
            .and_then(|v| v.as_str())
            .cmp(&b.get("path").and_then(|v| v.as_str()))
    });
    entries
}

#[allow(clippy::too_many_arguments)]
fn write_manifest(
    stage: &Path,
    flavor: BuildFlavor,
    runtime_version: &str,
    fingerprint: &str,
    flags: &[String],
    toolchain: &BTreeMap<String, String>,
    cuda_archs: &str,
    probe: &ProbeResult,
    import_check: &str,
) {
    let files = manifest_file_entries(stage);
    let licenses: Vec<serde_json::Value> = files
        .iter()
        .filter(|file| file.get("kind").and_then(|v| v.as_str()) == Some("license"))
        .map(|file| {
            serde_json::json!({
                "path": file.get("path").and_then(|v| v.as_str()).unwrap_or_default(),
                "license": "See the bundled notice and source pin",
            })
        })
        .collect();
    let manifest = serde_json::json!({
        "schema": MANIFEST_SCHEMA,
        "protocol_version": 1,
        "artifact_id": flavor.artifact_id(),
        "artifact_version": runtime_version,
        "artifact": {
            "id": flavor.artifact_id(),
            "version": runtime_version,
            "flavor": flavor.as_str(),
        },
        "platform": {"os": "windows", "architecture": "x86_64"},
        "dependencies": flavor.dependencies(),
        "packaging": flavor.packaging(),
        "source_pins": {
            "funasr_release_tag": FUNASR_RELEASE_TAG,
            "funasr_commit": FUNASR_COMMIT,
            "funasr_source_zip_sha256": FUNASR_ZIP_SHA256,
            "llama_cpp_commit": LLAMA_CPP_COMMIT,
            "protocol_header": "xtask/funasr-worker/blink_worker_protocol.h",
            "patches": PATCH_INPUTS.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        },
        "build": {
            "fingerprint": fingerprint,
            "generator": "Ninja",
            "cmake_flags": flags,
            "toolchain": toolchain,
            "cuda_architectures": cuda_archs,
            "dynamic_backend_loading": true,
        },
        "runtime_requirements": match flavor {
            BuildFlavor::Base => serde_json::json!({"backend": "cpu"}),
            BuildFlavor::Vulkan => serde_json::json!({
                "backend": "vulkan",
                "vulkan_loader": "host-driver-provided",
                "minimum_driver": "Vulkan 1.1"
            }),
            BuildFlavor::Cuda => serde_json::json!({
                "backend": "cuda",
                "minimum_driver": "see CUDA Toolkit compatibility matrix",
                "toolkit": toolchain.get("toolkit_version").cloned().unwrap_or_default()
            }),
        },
        "validation": {
            "cpu_self_test": if flavor == BuildFlavor::Base { probe.status } else { "not-run" },
            "backend_probe": probe.status,
            "backend_probe_exit_code": probe.exit_code,
            "backend_probe_output": probe.output,
            "hardware_test": if flavor == BuildFlavor::Base || probe.status == "passed" { "passed" } else { "pending" },
            "exe_import_check": import_check,
        },
        "licenses": licenses,
        "files": files,
        "manifest_excluded_from_file_hashes": true,
    });
    let path = stage.join(ARTIFACT_MANIFEST_NAME);
    std::fs::write(&path, serde_json::to_string_pretty(&manifest).unwrap())
        .unwrap_or_else(|e| panic!("写入 artifact manifest 失败（{}）: {e}", path.display()));
}

fn validate_artifact_dir(path: &Path, expected_id: Option<&str>) -> Result<(), String> {
    let manifest_path = path.join(ARTIFACT_MANIFEST_NAME);
    if !manifest_path.is_file() {
        return Err(format!("缺少 {}", manifest_path.display()));
    }
    let manifest = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("manifest JSON 无效: {e}"))?;
    if manifest.get("schema").and_then(|v| v.as_u64()) != Some(MANIFEST_SCHEMA) {
        return Err(format!("manifest schema 不是 {MANIFEST_SCHEMA}"));
    }
    let artifact_id = manifest
        .pointer("/artifact/id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "manifest 缺少 artifact.id".to_string())?;
    if let Some(expected) = expected_id
        && artifact_id != expected
    {
        return Err(format!(
            "artifact id 错误：期望 {expected}，实际 {artifact_id}"
        ));
    }
    let entries = manifest
        .get("files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "manifest 缺少 files 数组".to_string())?;
    let mut declared = BTreeSet::new();
    for entry in entries {
        let relative = entry
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "manifest file 缺少 path".to_string())?;
        let relative_path_obj = Path::new(relative);
        if relative_path_obj.is_absolute() || relative.split('/').any(|part| part == "..") {
            return Err(format!("manifest 路径不安全: {relative}"));
        }
        if !declared.insert(relative.to_string()) {
            return Err(format!("manifest 重复声明文件: {relative}"));
        }
        let file = path.join(relative_path_obj);
        if !file.is_file() {
            return Err(format!("manifest 声明文件缺失: {relative}"));
        }
        let actual_size = std::fs::metadata(&file).map_err(|e| e.to_string())?.len();
        let expected_size = entry
            .get("size_bytes")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| format!("manifest 缺少 size_bytes: {relative}"))?;
        if actual_size != expected_size {
            return Err(format!(
                "文件大小不符: {relative}，期望 {expected_size}，实际 {actual_size}"
            ));
        }
        let expected_hash = entry
            .get("sha256")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("manifest 缺少 sha256: {relative}"))?;
        let actual_hash = sha256_file(&file).map_err(|e| e.to_string())?;
        if actual_hash != expected_hash {
            return Err(format!("文件 SHA-256 不符: {relative}"));
        }
    }
    let actual: BTreeSet<String> = collect_files(path)
        .into_iter()
        .filter(|file| file.file_name().and_then(|n| n.to_str()) != Some(ARTIFACT_MANIFEST_NAME))
        .map(|file| relative_path(path, &file))
        .collect();
    if actual != declared {
        return Err(format!(
            "manifest 文件闭包不符：declared={declared:?}, actual={actual:?}"
        ));
    }
    Ok(())
}

/// 使用 `raw_arg` 绕过 Rust 的 Windows 参数转义——`cmd /s /c "..."` 语义
/// 要求整行作为一个带内嵌引号的参数原样传递。
fn run_cmd_line_capture(line: &str, cwd: &Path) -> (bool, String) {
    use std::os::windows::process::CommandExt;
    let output = Command::new("cmd")
        .raw_arg(format!("/d /s /c \"{line}\""))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("启动 cmd 失败: {e}"));
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

fn run_cmd_line(line: &str, cwd: &Path, desc: &str) {
    use std::os::windows::process::CommandExt;
    let status = Command::new("cmd")
        .raw_arg(format!("/d /s /c \"{line}\""))
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("{desc}: 启动 cmd 失败: {e}"));
    assert!(status.success(), "{desc} 失败 exit={status}");
}

#[cfg(test)]
mod tests {
    use super::{
        BASE_ARTIFACT_ID, CUDA_ARTIFACT_ID, MANIFEST_SCHEMA, PATCH_INPUTS, RUNTIME_VERSION,
        VULKAN_ARTIFACT_ID, format_cmake_flag, patched_source_fingerprint, probe_output_is_valid,
        validate_artifact_dir, validate_runtime_lock_value,
    };

    #[test]
    fn cmake_flags_quote_paths_with_spaces() {
        assert_eq!(
            format_cmake_flag(r#"-DCMAKE_MAKE_PROGRAM=C:\Program Files\Ninja\ninja.exe"#),
            r#"-DCMAKE_MAKE_PROGRAM="C:\Program Files\Ninja\ninja.exe""#
        );
        assert_eq!(
            format_cmake_flag("-DGGML_BACKEND_DL=ON"),
            "-DGGML_BACKEND_DL=ON"
        );
    }

    #[test]
    fn probe_protocol_is_parsed_from_stdout_only() {
        let stdout = r#"{"type":"backend_probe","ok":true,"requested_backend":"cpu","actual_backend":"cpu","graph":{"status":"success"}}"#;
        assert!(probe_output_is_valid(stdout, "cpu"));
        assert!(!probe_output_is_valid(
            &format!("{stdout}load_backend: diagnostic on stderr\n"),
            "cpu"
        ));
    }

    #[test]
    fn patched_source_fingerprint_tracks_patches_and_header() {
        let unique = format!(
            "blink-funasr-fingerprint-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let patches = root.join("patches");
        std::fs::create_dir_all(&patches).unwrap();
        for (patch, _) in PATCH_INPUTS {
            std::fs::write(patches.join(patch), format!("patch:{patch}\n")).unwrap();
        }
        let header = root.join("blink_worker_protocol.h");
        std::fs::write(&header, b"header-v1\n").unwrap();

        let original = patched_source_fingerprint(&patches, &header).unwrap();
        std::fs::write(patches.join(PATCH_INPUTS[0].0), b"patch:changed\n").unwrap();
        let patch_changed = patched_source_fingerprint(&patches, &header).unwrap();
        assert_ne!(original, patch_changed, "patch 变化必须使缓存失效");

        std::fs::write(&header, b"header-v2\n").unwrap();
        let header_changed = patched_source_fingerprint(&patches, &header).unwrap();
        assert_ne!(patch_changed, header_changed, "协议头变化必须使缓存失效");

        std::fs::remove_dir_all(root).unwrap();
    }

    fn fixture_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "blink-runtime-manifest-{label}-{}",
            std::process::id()
        ))
    }

    fn write_fixture_manifest(root: &std::path::Path) {
        let payload = root.join("ggml-cpu.dll");
        std::fs::write(&payload, b"cpu-backend-fixture").unwrap();
        let hash = super::sha256_file(&payload).unwrap();
        let manifest = serde_json::json!({
            "schema": MANIFEST_SCHEMA,
            "artifact": {"id": BASE_ARTIFACT_ID},
            "files": [{
                "path": "ggml-cpu.dll",
                "size_bytes": std::fs::metadata(&payload).unwrap().len(),
                "sha256": hash
            }]
        });
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn artifact_manifest_rejects_hash_tampering_and_extra_files() {
        let root = fixture_dir("tamper");
        std::fs::create_dir_all(&root).unwrap();
        write_fixture_manifest(&root);
        assert!(validate_artifact_dir(&root, Some(BASE_ARTIFACT_ID)).is_ok());

        std::fs::write(root.join("unexpected.dll"), b"not-declared").unwrap();
        assert!(validate_artifact_dir(&root, Some(BASE_ARTIFACT_ID)).is_err());
        std::fs::remove_file(root.join("unexpected.dll")).unwrap();
        std::fs::write(root.join("ggml-cpu.dll"), b"tampered").unwrap();
        assert!(validate_artifact_dir(&root, Some(BASE_ARTIFACT_ID)).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_lock_validator_requires_all_immutable_artifact_hashes() {
        let lock = serde_json::json!({
            "schema": 1,
            "runtime_version": RUNTIME_VERSION,
            "release_tag": format!("funasr-runtime-v{RUNTIME_VERSION}"),
            "platform": "windows",
            "architecture": "x86_64",
            "artifacts": {
                BASE_ARTIFACT_ID: {
                    "asset": format!("{BASE_ARTIFACT_ID}-v{RUNTIME_VERSION}.zip"),
                    "url": format!("https://example.invalid/download/funasr-runtime-v{RUNTIME_VERSION}/{BASE_ARTIFACT_ID}-v{RUNTIME_VERSION}.zip"),
                    "sha256": "a".repeat(64),
                    "depends_on": [],
                    "required": true
                },
                VULKAN_ARTIFACT_ID: {
                    "asset": format!("{VULKAN_ARTIFACT_ID}-v{RUNTIME_VERSION}.zip"),
                    "url": format!("https://example.invalid/download/funasr-runtime-v{RUNTIME_VERSION}/{VULKAN_ARTIFACT_ID}-v{RUNTIME_VERSION}.zip"),
                    "sha256": "b".repeat(64),
                    "depends_on": [BASE_ARTIFACT_ID],
                    "required": false
                },
                CUDA_ARTIFACT_ID: {
                    "asset": format!("{CUDA_ARTIFACT_ID}-v{RUNTIME_VERSION}.zip"),
                    "url": format!("https://example.invalid/download/funasr-runtime-v{RUNTIME_VERSION}/{CUDA_ARTIFACT_ID}-v{RUNTIME_VERSION}.zip"),
                    "sha256": "c".repeat(64),
                    "depends_on": [BASE_ARTIFACT_ID],
                    "required": false
                }
            }
        });
        assert!(validate_runtime_lock_value(&lock).is_empty());

        let mut incomplete = lock;
        incomplete["artifacts"][BASE_ARTIFACT_ID]["sha256"] = serde_json::Value::Null;
        let failures = validate_runtime_lock_value(&incomplete);
        assert!(failures.iter().any(|failure| failure.contains("base")));
    }
}
