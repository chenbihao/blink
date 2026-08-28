#!/usr/bin/env python3
"""
blink_model_installer.py — FunASR 模型安装 worker（0.22.6 H3/B2）

由 Rust 的 `FunasrModelInstallWorker` 作为受管子进程启动，负责：
- 使用 current generation venv 中的 Python 运行
- 使用 FunASR/ModelScope 官方库下载模型
- 只接受编译期 allowlist 中的 model id/revision
- MODELSCOPE_CACHE 指向本次 staging payload 目录
- staging 目录创建失败必须 fail closed
- 禁止回落到用户 ~/.cache/modelscope
- stdout/stderr 实时输出（Rust 侧通过 InstallSink 捕获）
- 取消/超时后 worker 及其子进程全部退出

worker 成功只代表下载完成；最终 fingerprint、manifest 与 promote 由 Rust 执行。

支持模型：
- iic/SenseVoiceSmall
- paraformer-zh

用法（由 Rust 调用，不接受外部参数）：
    python blink_model_installer.py \
        --model iic/SenseVoiceSmall \
        --revision funasr-1.x \
        --staging-dir /path/to/staging/payload

退出码：
    0 = 下载成功
    1 = 下载失败
    2 = 参数错误
    3 = staging 目录创建失败（fail closed）
"""

import argparse
import json
import os
import sys
import hashlib
import struct
from pathlib import Path
from typing import Optional

# ── UTF-8 安全（spec-backend §九）──────────────────────────────────────────
sys.stdin.reconfigure(encoding='utf-8', errors='replace')
sys.stdout.reconfigure(encoding='utf-8', errors='replace', line_buffering=True)
sys.stderr.reconfigure(encoding='utf-8', errors='replace')

# ── 编译期 allowlist ──────────────────────────────────────────────────────
# 只有此列表中的 model_id 被接受，其余 fail closed。
#
# 每个条目声明：
# - revision: 逻辑合同 revision（不是上游不可变 snapshot revision）
# - description: 人类可读描述
# - submodels: 非主模型之外的子模型列表（VAD/punc 等），None=不需要子模型
#
# installer 会下载主模型和所有声明的子模型到同一个 staging payload 目录，
# 确保产出自包含、可直接加载的 payload。
ALLOWED_MODELS = {
    "iic/SenseVoiceSmall": {
        "revision": "funasr-1.x",
        "description": "SenseVoice Small (五语种 ASR)",
        # SenseVoice 内置 VAD + 标点 + ITN，无需子模型
        "submodels": [],
    },
    "paraformer-zh": {
        "revision": "funasr-1.x",
        "description": "SeacoParaformer 中文 ASR",
        # Paraformer 需要 VAD 和标点子模型
        "submodels": ["fsmn-vad", "ct-punc"],
    },
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Blink FunASR 模型安装 worker"
    )
    parser.add_argument(
        "--model",
        required=True,
        help="模型 id（必须在 allowlist 中）",
    )
    parser.add_argument(
        "--revision",
        required=True,
        help="模型 revision（必须与 allowlist 匹配）",
    )
    parser.add_argument(
        "--staging-dir",
        required=True,
        help="staging payload 目录路径（MODELSCOPE_CACHE 将指向此目录）",
    )
    return parser.parse_args()


def verify_model(model_id: str, revision: str) -> None:
    """校验 model_id 和 revision 在 allowlist 中，否则 fail closed。"""
    if model_id not in ALLOWED_MODELS:
        print(f"[ERROR] model_id '{model_id}' 不在 allowlist 中", file=sys.stderr)
        sys.exit(2)
    expected_rev = ALLOWED_MODELS[model_id]["revision"]
    if revision != expected_rev:
        print(
            f"[ERROR] revision '{revision}' 与 allowlist 期望 '{expected_rev}' 不匹配",
            file=sys.stderr,
        )
        sys.exit(2)


def ensure_staging(staging_dir: str) -> Path:
    """确保 staging 目录存在，创建失败则 fail closed。"""
    path = Path(staging_dir)
    try:
        path.mkdir(parents=True, exist_ok=True)
    except Exception as e:
        print(f"[ERROR] staging 目录创建失败: {e}", file=sys.stderr)
        sys.exit(3)
    if not path.is_dir():
        print(f"[ERROR] staging 路径不是目录: {path}", file=sys.stderr)
        sys.exit(3)
    return path


def _download_single_model(
    model_id: str,
    staging_dir: Path,
    is_submodel: bool = False,
) -> str:
    """下载单个模型到 staging 目录。

    使用 FunASR AutoModel 触发 ModelScope 下载。
    MODELSCOPE_CACHE 已设置为 staging_dir，模型文件会落入其中。

    返回 ModelScope 解析后的实际模型 id（可能与输入不同，
    如 paraformer-zh → iic/speech_seaco_paraformer_large_...）。

    is_submodel: True 表示这是子模型（VAD/punc），不额外触发子模型下载。
    """
    from funasr import AutoModel

    print(
        f"[INFO] 下载{'子' if is_submodel else '主'}模型: {model_id}"
    )
    sys.stdout.flush()

    kwargs = {
        "model": model_id,
        "disable_update": True,
        "disable_progress_bar": True,
        "disable_log": True,
    }

    # 子模型不递归触发子模型下载
    if is_submodel:
        kwargs["disable_submodel"] = True

    model = AutoModel(**kwargs)

    # 尝试获取 resolved model id（AutoModel 内部可能做了短名解析）
    resolved_id = model_id
    try:
        # FunASR AutoModel 可能有 model_path 或 model_revision 属性
        if hasattr(model, "model_path"):
            resolved_id = str(getattr(model, "model_path"))
    except Exception:
        pass

    return resolved_id


def download_model(model_id: str, staging_dir: Path) -> dict:
    """下载主模型和所有声明的子模型到 staging 目录。

    MODELSCOPE_CACHE 指向 staging 目录，禁止回落到用户默认缓存。

    返回安装元数据 dict，供 Rust 写入 manifest。
    """
    # 设置 MODELSCOPE_CACHE 为 staging 目录（fail closed——不回落到默认缓存）。
    # 注意：不要设置 MODELSCOPE_DOMAIN——ModelScope SDK 将其非空值直接当作
    # API 主机名（默认 www.modelscope.cn），误设会使所有请求指向无效主机
    # 导致 DNS 解析失败、下载必然失败。用户环境若残留该变量同样强制清除。
    os.environ["MODELSCOPE_CACHE"] = str(staging_dir)
    os.environ.pop("MODELSCOPE_DOMAIN", None)

    print(f"[INFO] 开始下载模型: {model_id}")
    print(f"[INFO] MODELSCOPE_CACHE={staging_dir}")
    sys.stdout.flush()

    model_info = ALLOWED_MODELS[model_id]
    submodels = model_info.get("submodels", [])

    resolved_ids = {}

    try:
        from funasr import AutoModel  # noqa: F401  — 提前检查可用性

    except ImportError as e:
        print(f"[ERROR] funasr 包未安装: {e}", file=sys.stderr)
        sys.exit(1)

    try:
        # 下载主模型
        main_resolved = _download_single_model(model_id, staging_dir, is_submodel=False)
        resolved_ids["main"] = main_resolved

        # 下载所有声明的子模型
        for sub_id in submodels:
            sub_resolved = _download_single_model(sub_id, staging_dir, is_submodel=True)
            resolved_ids[sub_id] = sub_resolved

        # 确认 staging 目录非空
        if not staging_dir.exists() or not any(staging_dir.iterdir()):
            print(
                f"[ERROR] 模型下载后 staging 目录为空: {staging_dir}",
                file=sys.stderr,
            )
            sys.exit(1)

        print(f"[INFO] 模型下载完成: {model_id}")
        if submodels:
            print(f"[INFO] 子模型: {', '.join(submodels)}")
        print(f"[INFO] staging 目录: {staging_dir}")
        sys.stdout.flush()

        # 构建安装元数据
        # 注意：revision 是逻辑合同标识，不是上游不可变 snapshot revision。
        # 如果 ModelScope 返回了 resolved commit/snapshot identity，
        # 我们尝试在 resolved_ids 中记录它。
        metadata = {
            "model_id": model_id,
            "revision": model_info["revision"],
            "submodels": submodels,
            "resolved_ids": resolved_ids,
            "source_repo": "modelscope",
            "source_url": f"https://www.modelscope.cn/models/{model_id}",
        }

        return metadata

    except Exception as e:
        print(f"[ERROR] 模型下载失败: {e}", file=sys.stderr)
        import traceback
        traceback.print_exc(file=sys.stderr)
        sys.exit(1)


# ── Rust/Python 共享 canonical fingerprint ─────────────────────────────────
# 以下算法必须与 Rust 侧 model_storage.rs::compute_content_fingerprint 逐字节一致。
#
# 算法：
# 1. 递归枚举 payload 下的普通文件。
# 2. 使用相对于 payload 根目录的规范化 `/` 路径。
# 3. 按 UTF-8 相对路径字节排序。
# 4. 对每个文件依次哈希：
#    - 相对路径长度（u64 LE）与相对路径字节；
#    - 文件大小（u64 LE）；
#    - 文件内容。
# 5. 排除 Blink 自己的 manifest、current pointer、临时文件、下载锁和 staging 元数据。
# 6. 最终输出小写 64 位 hex SHA-256。


def _collect_files(root: Path, current: Path, files: list) -> None:
    """递归收集文件，排除 Blink 元数据文件。"""
    if not current.is_dir():
        return

    for entry in current.iterdir():
        name = entry.name

        # 排除 Blink 元数据文件
        if name in ("manifest.json", "current.json"):
            continue
        if name.startswith(".tmp_"):
            continue
        if name == ".download_lock":
            continue

        if entry.is_dir():
            _collect_files(root, entry, files)
        elif entry.is_file():
            # 计算相对路径，使用 `/` 分隔符
            rel = entry.relative_to(root)
            rel_str = "/".join(rel.parts)
            files.append((rel_str, entry))


def compute_content_fingerprint(payload_dir: Path) -> str:
    """计算目录的 content fingerprint（确定性目录聚合 SHA-256）。

    与 Rust 侧 model_storage.rs::compute_content_fingerprint 逐字节一致。
    """
    files: list = []
    _collect_files(payload_dir, payload_dir, files)

    # 按相对路径字节排序
    files.sort(key=lambda x: x[0].encode("utf-8"))

    hasher = hashlib.sha256()

    for rel_path, abs_path in files:
        rel_bytes = rel_path.encode("utf-8")
        rel_len = len(rel_bytes)

        # u64 LE: 相对路径长度
        hasher.update(struct.pack("<Q", rel_len))
        # 相对路径字节
        hasher.update(rel_bytes)

        # 读取文件
        with open(abs_path, "rb") as f:
            content = f.read()
        size = len(content)

        # u64 LE: 文件大小
        hasher.update(struct.pack("<Q", size))
        # 文件内容
        hasher.update(content)

    return hasher.hexdigest()


def main() -> None:
    args = parse_args()

    # 1. 校验 model_id 和 revision
    verify_model(args.model, args.revision)

    # 2. 确保 staging 目录存在（fail closed）
    staging_dir = ensure_staging(args.staging_dir)

    # 3. 下载主模型和所有子模型
    metadata = download_model(args.model, staging_dir)

    # 4. 输出诊断 fingerprint（Rust 是 manifest fingerprint 写入方）
    fp = compute_content_fingerprint(staging_dir)
    print(f"[FINGERPRINT] {fp}")
    sys.stdout.flush()

    # 5. 输出安装元数据 JSON（Rust 解析后写入 manifest）
    # 使用 [METADATA] 前缀标记，Rust 通过 InstallSink 捕获 stdout
    metadata_json = json.dumps(metadata, ensure_ascii=False)
    print(f"[METADATA] {metadata_json}")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
