#!/usr/bin/env python3
"""
blink_stt_server.py — Blink 自定义 STT 服务

支持：
- HTTP POST /v1/audio/transcriptions  ← 非流式（SenseVoice / SeacoParaformer），支持热词/ITN
- GET  /health                        ← 健康检查
- GET  /v1/models                     ← 模型列表

HTTP 端点路径和响应格式与官方 funasr-server 完全兼容，
现有 Rust 侧的 LocalSttEngine / PseudoStreamingSttEngine 和 is_server_ready_http() 无需修改。

模型 lazy load：只有实际收到请求时才加载对应模型。

0.22.6: 模型加载改为只从 BLINK_MODEL_PAYLOAD_DIR 指向的本地 payload 目录加载，
不触发隐式网络下载。payload 目录不存在或模型文件缺失时直接报错。

用法:
    python blink_stt_server.py --model SenseVoiceSmall --port 8000 --device cpu
    python blink_stt_server.py --model SenseVoiceSmall --port 8000 --device cpu \
        --use-itn --hotwords /path/to/hotwords.txt
"""

import argparse
import hashlib
import io
import json
import os
import struct
import sys
import tempfile
import traceback
import wave
import logging
from pathlib import Path
from typing import Optional

import numpy as np
import uvicorn
from fastapi import FastAPI, UploadFile, File, Form, Request
from fastapi.responses import JSONResponse

# ── 全局状态 ──────────────────────────────────────────────────────────────

app = FastAPI(title="Blink STT Server", version="0.10.4")

# 全局模型实例（lazy load）
_model = None          # 非流式模型（SenseVoice / SeacoParaformer）
_model_lock = None     # threading.Lock

# 模型加载状态："idle" → "loading" → "ready" / "error"
# 供 /health 端点暴露给 Rust 侧，使启动流程能区分
# "FastAPI 已就绪但模型还在下载" 与 "模型已就绪可推理"。
_model_status = "idle"

# 0.22.6 H1/B2: 模型内容指纹（model_content_fingerprint）
# 模型 Ready 时计算，基于 payload 目录的 canonical fingerprint。
# 与 Rust model_storage.rs::compute_content_fingerprint 逐字节一致。
# 用于检测模型文件损坏/篡改。非空、非全零。
_model_content_fingerprint: Optional[str] = None

# 0.22.6 B2: 动态模型身份（从环境变量注入，不使用静态 descriptor 合同）
# BLINK_MODEL_ID: 本次启动的 canonical model id
# BLINK_MODEL_REVISION: 本次启动的 model revision
# BLINK_MODEL_PAYLOAD_DIR: 已安装 generation 的 payload 目录路径
# BLINK_MODEL_FINGERPRINT: manifest 中记录的 fingerprint（用于运行时比对）
# BLINK_MODEL_SUBMODELS: 逗号分隔的子模型 id 列表（如 "fsmn-vad,ct-punc"）
_model_id_expected: Optional[str] = None
_model_revision_expected: Optional[str] = None
_model_payload_dir: Optional[str] = None
_model_fingerprint_expected: Optional[str] = None
_model_submodels: list = []

# 启动参数
_args: Optional[argparse.Namespace] = None

logger = logging.getLogger("blink_stt_server")


def get_args() -> argparse.Namespace:
    global _args
    if _args is None:
        raise RuntimeError("Server not initialized: _args is None")
    return _args


def _token_fingerprint(token: str) -> str:
    """计算 token 的 fingerprint（SHA-256 前 16 hex 字符）。

    0.22.3 Task D: 改为 SHA-256 固定前缀，与 Rust 侧 `token_fingerprint` 一致。
    日志中只能记录 fingerprint，不能记录明文 token。
    """
    h = hashlib.sha256(token.encode("utf-8")).hexdigest()
    return f"fp:{h[:16]}"


# ── 0.22.3 Task D: Token 认证 ─────────────────────────────────────────────

def _get_engine_token() -> Optional[str]:
    """从环境变量读取引擎 token。

    0.22.3 Task D: token 不通过命令行参数暴露，只从环境变量读取。
    """
    return os.environ.get("BLINK_ENGINE_TOKEN")


def _get_engine_id() -> str:
    """从环境变量读取引擎 id。"""
    return os.environ.get("BLINK_ENGINE_ID", "funasr")


def _get_instance_id() -> Optional[str]:
    """从环境变量读取实例 id。"""
    return os.environ.get("BLINK_INSTANCE_ID")


def _verify_token(request: Request) -> bool:
    """验证请求中的 X-Engine-Token header。

    0.22.3 Task G: fail closed——没有 BLINK_ENGINE_TOKEN 时拒绝所有请求。
    token 缺失或不匹配返回 False（调用方返回 401）。
    token 不写日志。
    """
    engine_token = _get_engine_token()
    if not engine_token:
        # 未配置 token——fail closed，不允许任何请求
        return False

    request_token = request.headers.get("X-Engine-Token")
    if not request_token:
        return False

    return request_token == engine_token


# ── 模型名解析 ────────────────────────────────────────────────────────────

# FunASR 短名 → ModelScope 完整 ID 映射。
# FunASR 1.3.14 的 AutoModel 内置短名解析在某些场景下会失效
#（ModelScope API 返回 404），因此在这里显式映射到完整 ID。
_MODEL_ALIASES = {
    "SenseVoiceSmall": "iic/SenseVoiceSmall",
    "sensevoice": "iic/SenseVoiceSmall",
    "SenseVoice": "iic/SenseVoiceSmall",
    # paraformer-zh 短名交给 FunASR 内部的 name_maps_ms 解析，
    # 它会映射到 SeacoParaformer（iic/speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch），
    # 这是原生支持热词的 Paraformer 变体。
    # 不在 _MODEL_ALIASES 中映射，避免覆盖 FunASR 的正确解析。
}


def _resolve_model_id(name: str) -> str:
    """将 FunASR 短名解析为 ModelScope 完整 ID。

    - 已知短名 → 完整 ID（如 SenseVoiceSmall → iic/SenseVoiceSmall）
    - 已含命名空间（如 iic/SenseVoiceSmall）→ 原样返回
    - 本地路径 → 原样返回
    - 未知短名 → 原样返回（让 FunASR/ModelScope 自行处理）
    """
    if not name:
        return name
    if name in _MODEL_ALIASES:
        resolved = _MODEL_ALIASES[name]
        logger.info(f"模型名解析: {name} → {resolved}")
        return resolved
    return name


# ── 0.22.6 H1: 模型内容指纹 ────────────────────────────────────────────────

# ── 0.22.6 B2: Rust/Python 共享 canonical fingerprint ──────────────────────
#
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


def _compute_model_content_fingerprint() -> Optional[str]:
    """计算当前 payload 目录的 content fingerprint。

    0.22.6 B2: 使用 BLINK_MODEL_PAYLOAD_DIR 指向的 payload 目录，
    不再扫描 ModelScope 缓存目录。
    """
    payload_dir = _model_payload_dir
    if not payload_dir:
        return None
    payload_path = Path(payload_dir)
    if not payload_path.is_dir():
        return None
    try:
        fp = compute_content_fingerprint(payload_path)
        if not fp or all(c == "0" for c in fp):
            return None
        return fp
    except Exception as e:
        logger.warning(f"计算模型内容指纹失败: {e}")
        return None


# ── 模型类型检测 ────────────────────────────────────────────────────────

def _is_sensevoice(model_name: str) -> bool:
    """判断模型是否为 SenseVoice。

    SenseVoice 内置 VAD + 标点 + ITN + 情感标签，不需要额外子模型。
    Paraformer / SeacoParaformer 没有内置这些功能，需要配置子模型。
    """
    name_lower = model_name.lower()
    return "sensevoice" in name_lower


# ── payload 路径解析 ──────────────────────────────────────────────────────

def _find_model_dir_in_payload(payload_dir: Path, model_id: str) -> Optional[Path]:
    """在 payload 目录中查找模型目录。

    ModelScope 下载的模型会以特定目录结构存放在 cache 中。
    通常路径形如：
      {cache}/iic/SenseVoiceSmall/  (rclone 结构)
    或
      {cache}/model_id/             (简化结构)

    我们搜索 payload 目录下的所有子目录，找到包含模型文件
    （.pt, .bin, config.yaml 等）的目录。
    """
    # 首先尝试 model_id 作为子目录路径
    candidate = payload_dir / model_id
    if candidate.is_dir() and _has_model_files(candidate):
        return candidate

    # 尝试 model_id 的最后一段作为目录名
    short_name = model_id.split("/")[-1]
    candidate = payload_dir / short_name
    if candidate.is_dir() and _has_model_files(candidate):
        return candidate

    # 递归搜索——找到第一个含模型文件的目录
    for entry in payload_dir.rglob("*"):
        if entry.is_dir() and _has_model_files(entry):
            # 确保目录名包含 model_id 的关键部分
            dir_name = entry.name.lower()
            key = short_name.lower()
            if key in dir_name or model_id.lower() in str(entry.relative_to(payload_dir)).lower():
                return entry

    return None


def _has_model_files(dir_path: Path) -> bool:
    """检查目录是否包含模型文件。"""
    model_extensions = {".pt", ".bin", ".onnx", ".yaml", ".json"}
    for entry in dir_path.iterdir():
        if entry.is_file() and entry.suffix.lower() in model_extensions:
            return True
    return False


def _resolve_local_model_path(payload_dir: Path, model_id: str) -> str:
    """解析模型在 payload 目录中的本地路径。

    如果找到对应目录，返回绝对路径。
    如果找不到，fail-closed 返回错误。

    这是关键的安全检查——确保 AutoModel 只使用本地 payload 中的模型文件，
    不触发隐式网络下载。
    """
    model_dir = _find_model_dir_in_payload(payload_dir, model_id)
    if model_dir is not None:
        return str(model_dir)

    # 无法在 payload 中找到模型——fail closed
    raise RuntimeError(
        f"模型 '{model_id}' 未在 payload 目录 '{payload_dir}' 中找到。"
        "请确保模型已正确安装。Blink 不允许隐式网络下载。"
    )


# ── 模型加载 ──────────────────────────────────────────────────────────

def _load_model():
    """加载模型，lazy load。

    0.22.6 B2: 从本地 payload 目录加载模型，不触发隐式下载。
    如果 payload 目录不存在或模型文件缺失，直接返回 error，
    不从远程 ModelScope 下载。

    模型类型自动适配子模型配置：
    - SenseVoice: 内置 VAD + 标点 + ITN，无需子模型
    - Paraformer / SeacoParaformer: 需配置 vad_model + punc_model
      - vad_model="fsmn-vad": 语音端点检测（~3MB）
      - punc_model="ct-punc": 标点恢复 + ITN（~1.1GB）

    0.22.6: VAD 和 punc 子模型也必须从 payload 本地路径加载，
    不允许 AutoModel 隐式下载子模型。

    加载状态通过全局 ``_model_status`` 跟踪（idle → loading → ready/error），
    供 /health 端点暴露给 Rust 侧判断服务是否真正可用。
    """
    global _model, _model_lock, _model_status, _model_content_fingerprint
    if _model is not None:
        return _model

    import threading
    if _model_lock is None:
        _model_lock = threading.Lock()

    with _model_lock:
        if _model is not None:
            return _model

        args = get_args()
        _model_status = "loading"
        logger.info(f"加载模型: {args.model}, device={args.device}")

        # 0.22.6 B2: 从本地 payload 目录加载，不触发隐式下载
        payload_dir = _model_payload_dir
        if not payload_dir or not os.path.isdir(payload_dir):
            _model_status = "error"
            logger.error(
                f"模型 payload 目录不存在: {payload_dir}。"
                "请先在设置页安装模型。"
            )
            raise RuntimeError(
                f"Model payload directory not found: {payload_dir}"
            )

        payload_path = Path(payload_dir)

        try:
            from funasr import AutoModel

            resolved_model = _resolve_model_id(args.model)

            # 解析主模型的本地路径
            main_model_path = _resolve_local_model_path(payload_path, resolved_model)

            kwargs = {
                "model": main_model_path,
                "device": args.device,
                "disable_update": True,  # 跳过更新检查，减少日志噪声
            }

            # Paraformer / SeacoParaformer 需要额外的 VAD 和标点子模型。
            # SenseVoice 内置了这些功能，不需要（也不应该）添加子模型。
            #
            # 0.22.6: 子模型也必须从 payload 本地路径加载。
            # 使用 BLINK_MODEL_SUBMODELS 环境变量获取子模型 id 列表，
            # 然后在 payload 目录中解析它们的本地路径。
            if not _is_sensevoice(args.model):
                # 从环境变量获取子模型列表
                submodels = _model_submodels

                if not submodels:
                    raise RuntimeError(
                        "非 SenseVoice 模型缺少 BLINK_MODEL_SUBMODELS；"
                        "拒绝使用短名回退以避免运行期隐式下载"
                    )

                logger.info(f"检测到非 SenseVoice 模型，配置子模型: {submodels}")
                vad_id = next((sm for sm in submodels if "vad" in sm), None)
                punc_id = next((sm for sm in submodels if "punc" in sm), None)
                if not vad_id or not punc_id:
                    raise RuntimeError(
                        "非 SenseVoice 模型必须同时声明 VAD 与标点子模型"
                    )

                vad_path = _resolve_local_model_path(payload_path, vad_id)
                punc_path = _resolve_local_model_path(payload_path, punc_id)
                kwargs["vad_model"] = vad_path
                kwargs["punc_model"] = punc_path
                logger.info(f"VAD 子模型本地路径: {vad_path}")
                logger.info(f"punc 子模型本地路径: {punc_path}")
            else:
                logger.info(f"检测到 SenseVoice 模型，使用内置 VAD/标点/ITN")

            _model = AutoModel(**kwargs)
            _model_status = "ready"
            # 0.22.6 B2: 模型 Ready 时计算 canonical fingerprint
            _model_content_fingerprint = _compute_model_content_fingerprint()
            logger.info(
                f"模型 {args.model} 加载完成, "
                f"content_fingerprint={_model_content_fingerprint[:16] if _model_content_fingerprint else 'None'}..."
            )
            return _model
        except Exception as e:
            _model_status = "error"
            logger.error(f"模型加载失败: {e}")
            raise


# ── 音频工具 ──────────────────────────────────────────────────────────────

def _wav_bytes_to_numpy(wav_bytes: bytes):
    """解析 WAV 字节为 numpy f32 数组（16kHz, mono）。"""
    with io.BytesIO(wav_bytes) as bio:
        with wave.open(bio, "rb") as wf:
            n_channels = wf.getnchannels()
            sampwidth = wf.getsampwidth()
            framerate = wf.getframerate()
            n_frames = wf.getnframes()
            raw = wf.readframes(n_frames)

    # 只支持 16-bit PCM
    if sampwidth != 2:
        raise ValueError(f"不支持的采样位宽: {sampwidth}, 仅支持 16-bit PCM")

    audio = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0

    # 如果是多声道，取第一声道
    if n_channels > 1:
        audio = audio[::n_channels]

    return audio, framerate


# ── 文本后处理 ───────────────────────────────────────────────────────────

import re as _re

# rich_transcription_postprocess 延迟导入缓存
_rich_postprocess_fn = None

# Emoji 和非语音字符的正则模式。
# SenseVoice 模型有时会在文本中插入 emoji（如 😊😄）或事件描述（如 (大笑)(掌声)），
# 这些在中文语音输入场景中不需要。
_EMOJI_PATTERN = _re.compile(
    "["
    "\U0001F600-\U0001F64F"  # emoticons
    "\U0001F300-\U0001F5FF"  # symbols & pictographs
    "\U0001F680-\U0001F6FF"  # transport & map symbols
    "\U0001F1E0-\U0001F1FF"  # flags (iOS)
    "\U00002700-\U000027BF"  # dingbats
    "\U0001F900-\U0001F9FF"  # supplemental symbols and pictographs
    "\U00002600-\U000026FF"  # miscellaneous symbols
    "\U0001FA00-\U0001FA6F"  # chess symbols
    "\U0001FA70-\U0001FAFF"  # symbols and pictographs extended-a
    "]",
    flags=_re.UNICODE,
)

# SenseVoice 事件描述（括号形式），如 (大笑)(掌声)(音乐)(噪音)
_EVENT_DESC_PATTERN = _re.compile(
    r"[\(（](?:大笑|小笑|掌声|音乐|噪音|哭泣|叹气|咳嗽|呼吸|背景音|无声|笑声|哭声|"
    r"欢呼声|尖叫声|说话声|敲击声|响铃声|爆竹声|狗叫声|猫叫声|鸟叫声|水声|风声|雷声|"
    r"引擎声|键盘声|电话铃声|门铃声|脚步声)"
    r"[\)）]"
)


# 中文字符之间的空格模式。
# Paraformer / SeacoParaformer 使用字符级 tokenizer，原始输出每个字之间都有空格：
#   "那 我 现 在 能 输 入 了 吗"
# SenseVoice 不存在此问题。
# 此正则匹配两个 CJK 字符之间的空白，仅删除空白，保留两端的字符。
_CJK_SPACE_PATTERN = _re.compile(
    r"(?<=[\u4e00-\u9fff\u3400-\u4dbf\u3040-\u30ff\uac00-\ud7af])"
    r"\s+"
    r"(?=[\u4e00-\u9fff\u3400-\u4dbf\u3040-\u30ff\uac00-\ud7af])"
)


def _postprocess_text(raw_text: str) -> str:
    """对模型原始输出做后处理。

    处理步骤：
    1. rich_transcription_postprocess：去除 SenseVoice 的 <|zh|><|NEUTRAL|> 等元数据标签
    2. 去除 emoji（SenseVoice 有时会插入）
    3. 去除事件描述如 (大笑)(掌声)（SenseVoice 有时会插入）
    4. 去除中文字符之间的空格（Paraformer 字符级 tokenizer 的副产物）
    """
    global _rich_postprocess_fn
    if _rich_postprocess_fn is None:
        try:
            from funasr.utils.postprocess_utils import rich_transcription_postprocess
            _rich_postprocess_fn = rich_transcription_postprocess
        except ImportError:
            logger.warning("无法导入 rich_transcription_postprocess，标签可能残留")
            _rich_postprocess_fn = lambda s: s  # noqa: E731
    text = _rich_postprocess_fn(raw_text)

    # 去除 emoji
    text = _EMOJI_PATTERN.sub("", text)

    # 去除事件描述（括号形式）
    text = _EVENT_DESC_PATTERN.sub("", text)

    # 去除中文字符之间的空格（Paraformer 字符级 tokenizer 副产物）
    text = _CJK_SPACE_PATTERN.sub("", text)

    # 清理可能残留的多余空格
    text = text.strip()

    return text


# ── HTTP 端点 ─────────────────────────────────────────────────────────────

@app.get("/health")
async def health(request: Request):
    """健康检查。返回模型加载状态 + 服务身份回显。

    0.22.3 Task D: 验证 X-Engine-Token header。token 缺失/不匹配返回 401。
    0.22.3 扩展：
    - 回显 engine_id / instance_id / token_fingerprint / endpoint
      供 Rust 侧 `ServiceIdentityInput::verify` 核对身份。
    - 旧版 server 不回显这些字段，Rust 侧兼容降级为 Unreachable。

    响应字段：
    - ``status``: 始终为 "ok"（HTTP 层面服务正常）
    - ``model_loaded``: 模型是否已加载完毕（兼容旧字段）
    - ``model_status``: 模型加载状态枚举
    - ``engine_id``: 引擎 id（从环境变量读取）
    - ``instance_id``: 实例 id（从环境变量读取）
    - ``token_fingerprint``: token 的 SHA-256 fingerprint
    - ``endpoint``: 实际监听端点
    - ``backend``: 推理后端（cpu/cuda）
    - ``device_name``: 设备名
    - ``model_id``: 模型 id
    - ``model_revision``: 模型版本
    - ``model_content_fingerprint``: 模型内容指纹（仅 Ready 时返回）
    """
    # 0.22.3 Task D: token 验证
    if not _verify_token(request):
        return JSONResponse({"error": "unauthorized"}, status_code=401)

    args = get_args()
    response = {
        "status": "ok",
        "model_loaded": _model is not None,
        "model_status": _model_status,
    }

    # 0.22.3 Task D: 从环境变量读取身份（不从命令行参数）
    engine_id = _get_engine_id()
    instance_id = _get_instance_id()
    engine_token = _get_engine_token()

    if engine_id:
        response["engine_id"] = engine_id
    if instance_id:
        response["instance_id"] = instance_id
    if engine_token:
        response["token_fingerprint"] = _token_fingerprint(engine_token)
    if hasattr(args, "port"):
        response["endpoint"] = f"127.0.0.1:{args.port}"

    # 推理后端信息
    if _model is not None:
        try:
            import torch
            if torch.cuda.is_available():
                response["backend"] = "cuda"
                response["device_name"] = torch.cuda.get_device_name(0)
            else:
                response["backend"] = "cpu"
                response["device_name"] = "CPU"
        except ImportError:
            response["backend"] = args.device
            response["device_name"] = args.device.upper()
    else:
        response["backend"] = args.device
        response["device_name"] = args.device.upper()

    # 0.22.6 B2: 动态模型身份（从环境变量注入，不使用静态 descriptor 合同）
    response["model_id"] = _model_id_expected or getattr(args, "model", "")
    response["model_revision"] = _model_revision_expected or "funasr-1.x"

    # 0.22.6 B2: 模型内容指纹（仅在 Ready 时返回）
    if _model_status == "ready":
        response["model_content_fingerprint"] = _model_content_fingerprint

    return JSONResponse(response)


@app.get("/v1/models")
async def list_models():
    """模型列表（兼容 OpenAI API 格式）。"""
    args = get_args()
    return {
        "object": "list",
        "data": [
            {
                "id": args.model,
                "object": "model",
                "owned_by": "blink",
            }
        ],
    }


@app.post("/v1/audio/transcriptions")
async def transcribe(
    request: Request,
    file: UploadFile = File(...),
    model: str = Form(default=""),
    hotword: Optional[str] = Form(default=None),
    use_itn: Optional[str] = Form(default=None),
):
    """非流式转录（OpenAI 兼容 API）。

    0.22.3 Task D: 验证 X-Engine-Token header。token 缺失/不匹配返回 401。

    支持 FunASR 增强参数（官方 server 未暴露）：
    - hotword: 热词字符串（每行 "词 权重"）
    - use_itn: ITN 开关 ("true"/"false")
    """
    # 0.22.3 Task D: token 验证
    if not _verify_token(request):
        return JSONResponse({"error": "unauthorized"}, status_code=401)

    args = get_args()

    # 读取上传的音频
    audio_bytes = await file.read()
    if not audio_bytes:
        return JSONResponse({"error": "empty audio"}, status_code=400)

    try:
        audio_np, sr = _wav_bytes_to_numpy(audio_bytes)
    except Exception as e:
        logger.error(f"WAV 解析失败: {e}")
        return JSONResponse({"error": f"WAV parse error: {e}"}, status_code=400)

    if len(audio_np) == 0:
        return {"text": ""}

    # 写临时 wav 文件（FunASR 的 generate 需要文件路径或 numpy）
    tmp_path = None
    try:
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
            tmp_path = tmp.name
            with wave.open(tmp, "wb") as wf:
                wf.setnchannels(1)
                wf.setsampwidth(2)
                wf.setframerate(16000)
                wf.writeframes((audio_np * 32767).astype(np.int16).tobytes())

        # 加载模型（lazy）
        stt_model = _load_model()

        # 构建 generate 参数
        gen_kwargs = {"input": tmp_path}

        # language 参数是 SenseVoice 专有的（减少英文语气词幻觉）。
        # Paraformer 不识别此参数，传了也不会报错但无效果。
        if _is_sensevoice(args.model):
            gen_kwargs["language"] = "zh"

        # 热词（优先请求参数，其次启动参数）
        # SeacoParaformer 原生支持热词 boosting；SenseVoice 对热词支持有限。
        hotword_str = hotword or args.hotwords
        if hotword_str:
            # 如果 hotword 是文件路径，直接传；否则写临时文件
            if os.path.isfile(hotword_str):
                gen_kwargs["hotword"] = hotword_str
            else:
                # 字符串热词，写临时文件
                with tempfile.NamedTemporaryFile(
                    suffix=".txt", delete=False, mode="w", encoding="utf-8"
                ) as hw_tmp:
                    hw_tmp.write(hotword_str)
                    hw_path = hw_tmp.name
                gen_kwargs["hotword"] = hw_path

        # ITN（逆文本归一化："二零二四年" → "2024年"）
        # SenseVoice 通过 <|withitn|> 标签内置 ITN；
        # Paraformer 的 ITN 由 punc_model (ct-punc) 提供。
        itn_val = True  # 默认开
        if use_itn is not None:
            itn_val = use_itn.lower() in ("true", "1", "yes")
        elif args.use_itn is not None:
            itn_val = args.use_itn
        gen_kwargs["use_itn"] = itn_val

        logger.info(f"非流式转录: samples={len(audio_np)}, model={args.model}, "
                     f"hotword={'yes' if gen_kwargs.get('hotword') else 'no'}, itn={gen_kwargs.get('use_itn')}")

        result = stt_model.generate(**gen_kwargs)

        # FunASR 返回 list[dict]，每个 dict 含 "text" 字段。
        # 当配置了 vad_model 时，长音频会被切成多段，每段一个 dict；
        # 无 vad_model 时通常只有一段。
        # 需拼接所有段的文本。
        if isinstance(result, list) and len(result) > 0:
            parts = [r.get("text", "") for r in result if isinstance(r, dict)]
            raw_text = "".join(parts)
        elif isinstance(result, dict):
            raw_text = result.get("text", "")
        else:
            raw_text = str(result)

        # 后处理：去除 SenseVoice 元数据标签 / emoji / CJK 间空格
        text = _postprocess_text(raw_text)

        logger.info(f"转录结果: {text[:100]}")
        return {"text": text}

    except Exception as e:
        logger.error(f"转录失败: {e}\n{traceback.format_exc()}")
        return JSONResponse({"error": str(e)}, status_code=500)

    finally:
        if tmp_path and os.path.exists(tmp_path):
            os.unlink(tmp_path)


# ── 启动 ──────────────────────────────────────────────────────────────────

from contextlib import asynccontextmanager

@asynccontextmanager
async def lifespan(app_instance):
    """FastAPI lifespan handler（替代废弃的 on_event('startup')）。"""
    args = get_args()

    logger.info("=" * 60)
    logger.info("Blink STT Server v0.10.4")
    logger.info(f"  模型: {args.model}")
    logger.info(f"  设备: {args.device}")
    logger.info(f"  端口: {args.port}")
    logger.info(f"  热词: {args.hotwords or '(无)'}")
    logger.info(f"  ITN:  {args.use_itn}")
    logger.info("=" * 60)

    # 预加载模型（后台，避免阻塞 server 启动）
    # 模型从本地 payload 加载，不触发网络下载。
    # _model_status 会在 _load_model 内部经历 idle → loading → ready/error，
    # Rust 侧通过 /health 轮询此状态，在模型就绪前不会报告服务 "ready"。
    import asyncio
    def _preload_model():
        try:
            _load_model()
        except Exception as e:
            logger.warning(f"模型后台预加载失败（将在首次请求时重试）: {e}")
    asyncio.get_event_loop().run_in_executor(None, _preload_model)

    yield  # server 运行中

    # shutdown（目前无需特殊清理）


app.router.lifespan_context = lifespan


def parse_args():
    parser = argparse.ArgumentParser(
        description="Blink STT Server (FunASR-based, 0.10.4)"
    )
    parser.add_argument(
        "--model", default="SenseVoiceSmall",
        help="模型标识（默认 SenseVoiceSmall）"
    )
    parser.add_argument(
        "--port", type=int, default=8000,
        help="监听端口"
    )
    parser.add_argument(
        "--device", default="cpu",
        help="推理设备: cpu 或 cuda"
    )
    parser.add_argument(
        "--hotwords", default=None,
        help="热词文件路径"
    )
    parser.add_argument(
        "--use-itn", action="store_true", default=True,
        help="启用 ITN 逆文本归一化"
    )
    # 0.22.3 Task G: 身份参数已移除 CLI，只通过环境变量传入
    # BLINK_ENGINE_TOKEN / BLINK_ENGINE_ID / BLINK_INSTANCE_ID
    return parser.parse_args()


def _init_dynamic_model_identity():
    """从环境变量读取动态模型身份（0.22.6 B2）。

    启动身份从 Manifest 动态获取，而非静态 Descriptor 硬编码。
    Rust adapter 在 prepare_launch 时从 model_storage manifest 读取
    model_id / revision / payload_dir / fingerprint，通过环境变量注入。

    环境变量：
    - BLINK_MODEL_ID: canonical model id（如 iic/SenseVoiceSmall）
    - BLINK_MODEL_REVISION: model revision
    - BLINK_MODEL_PAYLOAD_DIR: 已安装 generation 的 payload 目录绝对路径
    - BLINK_MODEL_FINGERPRINT: manifest 中记录的 fingerprint（用于运行时比对）
    - BLINK_MODEL_SUBMODELS: 逗号分隔的子模型 id 列表（如 "fsmn-vad,ct-punc"）

    如果 BLINK_MODEL_PAYLOAD_DIR 缺失，model_status 将在 _load_model 时报错，
    不触发隐式网络下载。
    """
    global _model_id_expected, _model_revision_expected
    global _model_payload_dir, _model_fingerprint_expected, _model_submodels

    _model_id_expected = os.environ.get("BLINK_MODEL_ID")
    _model_revision_expected = os.environ.get("BLINK_MODEL_REVISION")
    _model_payload_dir = os.environ.get("BLINK_MODEL_PAYLOAD_DIR")
    _model_fingerprint_expected = os.environ.get("BLINK_MODEL_FINGERPRINT")

    submodels_str = os.environ.get("BLINK_MODEL_SUBMODELS", "")
    if submodels_str:
        _model_submodels = [
            s.strip() for s in submodels_str.split(",") if s.strip()
        ]
    else:
        _model_submodels = []

    logger.info("=" * 60)
    logger.info("动态模型身份（从 Manifest 注入）:")
    logger.info(f"  model_id:       {_model_id_expected or '(未注入)'}")
    logger.info(f"  revision:       {_model_revision_expected or '(未注入)'}")
    logger.info(f"  payload_dir:    {_model_payload_dir or '(未注入)'}")
    logger.info(
        f"  fingerprint:    "
        f"{_model_fingerprint_expected[:16] + '...' if _model_fingerprint_expected else '(未注入)'}"
    )
    logger.info(f"  submodels:      {_model_submodels or '(无)'}")
    logger.info("=" * 60)

    # fail-closed: payload_dir 缺失时记录警告，_load_model 会报错
    if not _model_payload_dir:
        logger.warning(
            "BLINK_MODEL_PAYLOAD_DIR 未注入——模型加载将失败。"
            "请确保模型已正确安装。"
        )


if __name__ == "__main__":
    # 配置日志
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
        datefmt="%H:%M:%S",
        stream=sys.stdout,
    )

    # 初始化动态模型身份（在 parse_args 之前）
    _init_dynamic_model_identity()
    _args = parse_args()

    # 启动 server（绑定 127.0.0.1，loopback only）
    import uvicorn
    uvicorn.run(
        app,
        host="127.0.0.1",
        port=_args.port,
        log_level="info",
    )
