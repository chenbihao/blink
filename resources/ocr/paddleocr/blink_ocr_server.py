#!/usr/bin/env python3
"""PP-OCRv6 生产 HTTP wrapper — Blink OCR 本地引擎（0.22.4）。

基于 spike server_thin.py（拓扑 A），适配 LocalEngineService 的 LaunchContext 协议：
- --engine-id / --instance-id / --token 由 adapter 从 LaunchContext 传入
- /health 返回 engine_id / instance_id 供身份验证
- 模型档位默认 tiny（唯一通过生产资格门的候选）

协议版本：0.3.0（0.22.4 正式版，基于 spike 0.2.0 冻结草案）

用法：
    python blink_ocr_server.py --port 9100 --model tiny --token <random> \
        --engine-id paddleocr --instance-id <uuid> --model-cache ./model-cache
"""

import argparse
import hashlib
import io
import json
import os
import struct
import sys
import threading
import time
import traceback
import uuid

# UTF-8 安全（spec-backend §九）
sys.stdin.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace", line_buffering=True)
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

# rect 归一化 seam（纯 stdlib，与 test_ocr_rect.py 共用同一套生产实现）
from ocr_rect import extract_results

# ── OCR 输入资源预算（0.22.6.1，与 Rust 侧 input_budget.rs 契约锁定） ──
# Rust 测试会校验本文件中的这三行字面量，防止两侧静默漂移。
MAX_BODY_BYTES = 32 * 1024 * 1024      # compressed/input bytes ≤ 32 MiB
MAX_DIMENSION = 16384                  # 单边 ≤ 16384 px
MAX_DECODED_BYTES = 256 * 1024 * 1024  # decoded RGB/RGBA 预算 ≤ 256 MiB（按 4 通道计）

# ── 模型身份常量（/health 与 /recognize 使用同一套常量） ──

ENGINE_NAME = "paddleocr"
MODEL_REVISION = "ppocrv6-tiny"

# 模型档位 → 官方模型名映射
MODEL_MAP = {
    "tiny": {
        "det": "PP-OCRv6_tiny_det",
        "rec": "PP-OCRv6_tiny_rec",
    },
    "small": {
        "det": "PP-OCRv6_small_det",
        "rec": "PP-OCRv6_small_rec",
    },
    "medium": {
        "det": "PP-OCRv6_medium_det",
        "rec": "PP-OCRv6_medium_rec",
    },
}


def model_id_for(tier):
    """根据模型档位生成 model_id 字符串。"""
    names = MODEL_MAP.get(tier, MODEL_MAP["tiny"])
    return f"PP-OCRv6:{names['det']}:{names['rec']}"


def _parse_png_header(png_bytes):
    """解析 PNG header（signature + IHDR），header 级尺寸检查用。

    完整解码之前执行——避免在完成尺寸预算检查前进行无界像素解码。

    返回 ``(error, (width, height))``：error 为 None 表示 header 合法。
    """
    if len(png_bytes) < 24:
        return ("body shorter than PNG header", (0, 0))
    PNG_SIG = b"\x89PNG\r\n\x1a\n"
    if png_bytes[:8] != PNG_SIG:
        return ("invalid PNG signature", (0, 0))
    if png_bytes[12:16] != b"IHDR":
        return ("missing IHDR chunk", (0, 0))
    width, height = struct.unpack(">II", png_bytes[16:24])
    if width == 0 or height == 0:
        return ("zero image dimensions", (0, 0))
    return (None, (width, height))


# ── 全局状态 ──

_ENGINE = None
_ENGINE_LOCK = threading.Lock()  # 保护模型初始化的 single-flight
_TOKEN = None
_ENGINE_ID = "paddleocr"
_INSTANCE_ID = str(uuid.uuid4())
_START_TIME = time.time()
_MODEL_STATE = "NotLoaded"  # NotLoaded | Loading | Ready | Failed
_MODEL_ID = model_id_for("tiny")
_MODEL_CONTENT_FINGERPRINT = None
_ENDPOINT = None  # 由 main() 从 --host/--port 构建
_init_start_time = None
_http_reachable_time = None
_model_ready_time = None
_SERVER = None

# 全局 args（在 main 中赋值）
args = None


def _set_model_state(state, fingerprint=None):
    global _MODEL_STATE, _MODEL_CONTENT_FINGERPRINT
    _MODEL_STATE = state
    if fingerprint is not None:
        _MODEL_CONTENT_FINGERPRINT = fingerprint
    print(f"[STATE] model_state={state} fingerprint={_MODEL_CONTENT_FINGERPRINT}", flush=True)


# ── 模型内容指纹计算 ──

# PaddleOCR 3.7 / PaddleX 模型缓存的目录结构（基于完整相对路径识别）：
#   model_cache_dir/
#     official_models/
#       PP-OCRv6_tiny_det/      ← det 模型目录
#         inference.pdmodel
#         inference.pdparams
#         ...其他 det 相关文件
#       PP-OCRv6_tiny_rec/      ← rec 模型目录
#         inference.pdmodel
#         inference.pdparams
#         ...其他 rec 相关文件
#
# 基于完整相对路径识别：
# - 路径中包含 det 模型目录名（如 PP-OCRv6_tiny_det）→ det 模型文件
# - 路径中包含 rec 模型目录名（如 PP-OCRv6_tiny_rec）→ rec 模型文件
# - 不依赖文件名 basename 匹配，使用完整相对路径

def _is_target_model_file(rel_path, det_model_name, rec_model_name):
    """基于完整相对路径判断文件是否属于目标 det/rec 模型。

    检查相对路径中是否包含 det 或 rec 模型目录名。
    例如：
    - 'official_models/PP-OCRv6_tiny_det/inference.pdmodel' → True (det)
    - 'official_models/PP-OCRv6_tiny_rec/inference.pdmodel' → True (rec)
    - 'official_models/PP-OCRv6_small_det/inference.pdmodel' → False (档位不匹配)
    - 'some_other_dir/file.txt' → False
    """
    rel_lower = rel_path.lower().replace("\\", "/")
    det_lower = det_model_name.lower()
    rec_lower = rec_model_name.lower()
    return det_lower in rel_lower or rec_lower in rel_lower


def _classify_model(rel_path, det_model_name, rec_model_name):
    """分类文件属于 det 还是 rec 模型。

    返回 'det'、'rec' 或 None。
    """
    rel_lower = rel_path.lower().replace("\\", "/")
    if det_model_name.lower() in rel_lower:
        return "det"
    if rec_model_name.lower() in rel_lower:
        return "rec"
    return None


def _collect_model_files(model_cache_dir, det_model_name, rec_model_name):
    """收集目标 det/rec 模型文件，返回 [(rel_path, abs_path), ...]。

    使用完整相对路径匹配，不依赖 basename。
    """
    target_files = []

    if not os.path.exists(model_cache_dir):
        return []

    for root, dirs, files in os.walk(model_cache_dir):
        for f in files:
            fp = os.path.join(root, f)
            try:
                rel = os.path.relpath(fp, model_cache_dir)
                rel_normalized = rel.replace("\\", "/")
                if _is_target_model_file(rel_normalized, det_model_name, rec_model_name):
                    target_files.append((rel_normalized, fp))
            except OSError:
                pass

    # 按相对路径排序
    target_files.sort(key=lambda x: x[0])
    return target_files


def _compute_model_fingerprint(model_cache_dir, det_model_name, rec_model_name):
    """计算目标 det/rec 模型文件的内容指纹。

    按相对路径排序，同时对相对路径和文件内容做 SHA-256。
    不得使用文件数、总大小、mtime 冒充 fingerprint。

    返回 (fingerprint, file_list) 或 (None, [])。
    file_list 包含每个文件的路径和单文件 hash。
    """
    target_files = _collect_model_files(
        model_cache_dir, det_model_name, rec_model_name
    )

    if not target_files:
        return None, []

    # 检查同时包含 det 和 rec 模型
    has_det = any(
        _classify_model(rel, det_model_name, rec_model_name) == "det"
        for rel, _ in target_files
    )
    has_rec = any(
        _classify_model(rel, det_model_name, rec_model_name) == "rec"
        for rel, _ in target_files
    )
    if not has_det or not has_rec:
        print(
            f"[WARN] 模型文件不完整：det={has_det}, rec={has_rec}",
            file=sys.stderr,
            flush=True,
        )
        return None, []

    # 计算每个文件的 hash 和总 fingerprint
    hasher = hashlib.sha256()
    file_list = []

    for rel_path, abs_path in target_files:
        # hash 相对路径
        hasher.update(rel_path.encode("utf-8"))
        hasher.update(b"\x00")  # 分隔符
        # hash 文件内容
        file_hash = hashlib.sha256()
        try:
            with open(abs_path, "rb") as f:
                for chunk in iter(lambda: f.read(65536), b""):
                    hasher.update(chunk)
                    file_hash.update(chunk)
        except OSError as e:
            print(
                f"[WARN] 读取模型文件失败 {abs_path}: {e}",
                file=sys.stderr,
                flush=True,
            )
            return None, []
        hasher.update(b"\x00")  # 分隔符
        file_list.append({"path": rel_path, "sha256": file_hash.hexdigest()})

    return hasher.hexdigest(), file_list


# ── Manifest 读写 ──

MANIFEST_SCHEMA_VERSION = 1


def _manifest_path(model_cache_dir):
    """manifest.json 路径。"""
    return os.path.join(model_cache_dir, "manifest.json")


def _write_manifest(model_cache_dir, tier):
    """模型加载成功后写 manifest，包含模型身份和文件清单。

    manifest 包含：
    - schema_version
    - model_id / revision
    - det/rec 官方模型名
    - 文件清单及单文件 hash
    - 总 fingerprint

    如果文件清单为空或缺少 det/rec，不写 manifest 并返回 False。
    """
    names = MODEL_MAP.get(tier, MODEL_MAP["tiny"])
    fingerprint, file_list = _compute_model_fingerprint(
        model_cache_dir, names["det"], names["rec"]
    )
    if fingerprint is None:
        print(
            "[WARN] 无法计算模型 fingerprint，不写 manifest",
            file=sys.stderr,
            flush=True,
        )
        return False

    # file_list 已按路径排序（_compute_model_fingerprint 内部排序）

    manifest = {
        "schema_version": MANIFEST_SCHEMA_VERSION,
        "model_id": model_id_for(tier),
        "model_revision": MODEL_REVISION,
        "det_model_name": names["det"],
        "rec_model_name": names["rec"],
        "files": file_list,
        "model_content_fingerprint": fingerprint,
    }

    manifest_path = _manifest_path(model_cache_dir)
    try:
        with open(manifest_path, "w", encoding="utf-8") as f:
            json.dump(manifest, f, indent=2, ensure_ascii=False)
        print(f"[INFO] manifest 已写入 {manifest_path}", flush=True)
        return True
    except OSError as e:
        print(f"[WARN] 写 manifest 失败: {e}", file=sys.stderr, flush=True)
        return False


def _verify_manifest(model_cache_dir, tier):
    """后续启动校验 manifest 和实际文件。

    检查：
    - manifest 存在且可读
    - schema_version 正确
    - model_id / model_revision 与预期一致
    - det_model_name / rec_model_name 与预期一致
    - 文件清单非空
    - 同时包含 det 和 rec 模型文件
    - 每个文件存在且 hash 匹配
    - 总 fingerprint 与实际一致
    - fingerprint 为 64 位小写 hex

    返回 (valid, fingerprint)。
    fingerprint 为 None 表示无法验证（manifest 不存在或损坏）。
    """
    names = MODEL_MAP.get(tier, MODEL_MAP["tiny"])
    manifest_path = _manifest_path(model_cache_dir)
    if not os.path.exists(manifest_path):
        return (False, None)

    try:
        with open(manifest_path, "r", encoding="utf-8") as f:
            manifest = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        print(f"[WARN] manifest 损坏: {e}", file=sys.stderr, flush=True)
        return (False, None)

    # 检查 schema_version
    if manifest.get("schema_version") != MANIFEST_SCHEMA_VERSION:
        print(
            f"[WARN] manifest schema_version 不匹配: expected={MANIFEST_SCHEMA_VERSION}",
            file=sys.stderr,
            flush=True,
        )
        return (False, None)

    # 检查身份字段
    expected_model_id = model_id_for(tier)
    if manifest.get("model_id") != expected_model_id:
        print(
            f"[WARN] manifest model_id 不匹配: expected={expected_model_id}, "
            f"got={manifest.get('model_id')}",
            file=sys.stderr,
            flush=True,
        )
        return (False, None)

    if manifest.get("model_revision") != MODEL_REVISION:
        print(
            f"[WARN] manifest model_revision 不匹配: expected={MODEL_REVISION}, "
            f"got={manifest.get('model_revision')}",
            file=sys.stderr,
            flush=True,
        )
        return (False, None)

    # 检查 det/rec 模型名
    if manifest.get("det_model_name") != names["det"]:
        print(
            f"[WARN] manifest det_model_name 不匹配: expected={names['det']}",
            file=sys.stderr,
            flush=True,
        )
        return (False, None)

    if manifest.get("rec_model_name") != names["rec"]:
        print(
            f"[WARN] manifest rec_model_name 不匹配: expected={names['rec']}",
            file=sys.stderr,
            flush=True,
        )
        return (False, None)

    stored_fingerprint = manifest.get("model_content_fingerprint")
    if not stored_fingerprint:
        print("[WARN] manifest 缺少 model_content_fingerprint", file=sys.stderr, flush=True)
        return (False, None)

    # 验证 fingerprint 格式：64 位小写 hex
    if len(stored_fingerprint) != 64 or not all(
        c in "0123456789abcdef" for c in stored_fingerprint
    ):
        print(
            f"[WARN] fingerprint 格式无效（应为 64 位小写 hex）",
            file=sys.stderr,
            flush=True,
        )
        return (False, None)

    # 验证文件清单非空
    files = manifest.get("files", [])
    if not files:
        print("[WARN] manifest 文件清单为空", file=sys.stderr, flush=True)
        return (False, None)

    # 验证同时包含 det 和 rec 模型文件
    has_det = any(
        _classify_model(entry.get("path", ""), names["det"], names["rec"]) == "det"
        for entry in files
    )
    has_rec = any(
        _classify_model(entry.get("path", ""), names["det"], names["rec"]) == "rec"
        for entry in files
    )
    if not has_det or not has_rec:
        print(
            f"[WARN] manifest 文件清单缺少 det 或 rec 模型文件: det={has_det}, rec={has_rec}",
            file=sys.stderr,
            flush=True,
        )
        return (False, None)

    # 验证每个文件
    for entry in files:
        rel_path = entry.get("path", "")
        expected_hash = entry.get("sha256", "")
        abs_path = os.path.join(model_cache_dir, rel_path.replace("/", os.sep))

        if not os.path.exists(abs_path):
            print(f"[WARN] 模型文件缺失: {rel_path}", file=sys.stderr, flush=True)
            return (False, None)

        file_hash = hashlib.sha256()
        try:
            with open(abs_path, "rb") as f:
                for chunk in iter(lambda: f.read(65536), b""):
                    file_hash.update(chunk)
        except OSError:
            return (False, None)

        if file_hash.hexdigest() != expected_hash:
            print(f"[WARN] 模型文件 hash 不匹配: {rel_path}", file=sys.stderr, flush=True)
            return (False, None)

    # 验证总 fingerprint
    actual_fingerprint, _ = _compute_model_fingerprint(
        model_cache_dir, names["det"], names["rec"]
    )
    if actual_fingerprint != stored_fingerprint:
        print("[WARN] model_content_fingerprint 不匹配", file=sys.stderr, flush=True)
        return (False, None)

    return (True, stored_fingerprint)


def init_engine():
    """初始化 PaddleOCR 3.7 引擎。single-flight：只执行一次。"""
    global _ENGINE, _init_start_time, _model_ready_time, _MODEL_ID

    with _ENGINE_LOCK:
        if _ENGINE is not None:
            return _ENGINE

        _init_start_time = time.perf_counter()
        _set_model_state("Loading")

        try:
            from paddleocr import PaddleOCR  # type: ignore

            model_names = MODEL_MAP.get(args.model, MODEL_MAP["tiny"])
            _MODEL_ID = model_id_for(args.model)

            # PaddleOCR 3.7 API：
            # - ocr_version="PP-OCRv6" 使用官方代际值
            # - text_detection/recognition_model_name 指定具体模型
            # - use_doc_orientation_classify=False（截图不需要）
            # - use_doc_unwarping=False
            # - use_textline_orientation=False（截图不旋转）
            # - return_word_box=True 获取 word 级 boxes
            # - device="cpu" 强制 CPU
            _ENGINE = PaddleOCR(
                ocr_version="PP-OCRv6",
                text_detection_model_name=model_names["det"],
                text_recognition_model_name=model_names["rec"],
                use_doc_orientation_classify=False,
                use_doc_unwarping=False,
                use_textline_orientation=False,
                return_word_box=True,
                device="cpu",
                cpu_threads=args.cpu_threads,
                enable_mkldnn=args.enable_mkldnn,
            )

            _model_ready_time = time.perf_counter()

            # 计算模型内容指纹并写 manifest
            fingerprint, _ = _compute_model_fingerprint(
                args.model_cache, model_names["det"], model_names["rec"]
            )
            manifest_ok = _write_manifest(args.model_cache, args.model)

            if fingerprint is None or not manifest_ok:
                # fingerprint 生成失败时，模型不得进入 Ready
                _set_model_state("Failed")
                print(
                    "[ERROR] 模型 fingerprint 生成失败或 manifest 写入失败，"
                    "模型不进入 Ready 状态",
                    file=sys.stderr,
                    flush=True,
                )
                return _ENGINE

            _set_model_state("Ready", fingerprint)

            load_ms = round((_model_ready_time - _init_start_time) * 1000, 2)
            print(f"[INFO] 模型加载完成: {args.model} ({load_ms}ms)", flush=True)

        except Exception as e:
            _set_model_state("Failed")
            print(f"[ERROR] 模型加载失败: {e}", file=sys.stderr, flush=True)
            raise

    return _ENGINE


def init_engine_background():
    """在后台线程初始化模型，不阻塞服务启动。"""
    def _bg_init():
        try:
            init_engine()
        except Exception:
            # 错误已在 init_engine 中记录
            pass

    t = threading.Thread(target=_bg_init, daemon=True)
    t.start()
    return t


# ── HTTP 服务 ──

try:
    from fastapi import FastAPI, Header, HTTPException, Query, Request
except ImportError:
    print("[FATAL] FastAPI 未安装", file=sys.stderr)
    sys.exit(1)

app = FastAPI(title="Blink PP-OCRv6 OCR Server")


def verify_token(x_engine_token: str = Header(default=None, alias="X-Engine-Token")):
    """验证请求 token。"""
    if not _TOKEN:
        raise HTTPException(status_code=503, detail="service_not_ready")
    if x_engine_token != _TOKEN:
        raise HTTPException(status_code=401, detail="invalid_token")


@app.on_event("startup")
async def startup_event():
    """服务启动后在后台初始化模型。"""
    global _http_reachable_time
    _http_reachable_time = time.perf_counter()
    print(f"[INFO] HTTP reachable at {_http_reachable_time:.2f}s", flush=True)

    # 启动时先校验已有 manifest（如果存在）
    valid, fingerprint = _verify_manifest(args.model_cache, args.model)
    if valid:
        print(f"[INFO] manifest 校验通过，fingerprint={fingerprint[:16]}...", flush=True)
        global _MODEL_CONTENT_FINGERPRINT
        _MODEL_CONTENT_FINGERPRINT = fingerprint
    elif os.path.exists(_manifest_path(args.model_cache)):
        print("[WARN] manifest 校验失败，模型可能损坏，将重新加载", file=sys.stderr, flush=True)

    init_engine_background()


@app.get("/health")
async def health(x_engine_token: str = Header(default=None, alias="X-Engine-Token")):
    """健康检查。返回 engine_id / instance_id 供身份验证。"""
    if not _TOKEN:
        raise HTTPException(status_code=503, detail="service_not_ready")
    if x_engine_token != _TOKEN:
        raise HTTPException(status_code=401, detail="invalid_token")

    # token fingerprint：与 Rust port::token_fingerprint() 完全一致
    # 格式：fp: + SHA-256 前 8 字节的小写 16 hex
    # 不泄露完整 token
    if _TOKEN:
        hash_bytes = hashlib.sha256(_TOKEN.encode("utf-8")).digest()[:8]
        token_fp = "fp:" + hash_bytes.hex()
    else:
        token_fp = None

    return {
        "protocol_version": "0.3.0",
        "engine_id": _ENGINE_ID,
        "instance_id": _INSTANCE_ID,
        "token_fingerprint": token_fp,
        "endpoint": _ENDPOINT,
        "service_state": "healthy",
        "model_state": _MODEL_STATE,
        "model_id": _MODEL_ID,
        "model_revision": MODEL_REVISION,
        "model_content_fingerprint": _MODEL_CONTENT_FINGERPRINT,
        "model_detail": f"{args.model}:{MODEL_MAP.get(args.model, {}).get('det', '?')}+{MODEL_MAP.get(args.model, {}).get('rec', '?')}",
        "actual_backend": "cpu",
        "device_name": "CPU",
        "uptime_seconds": round(time.time() - _START_TIME, 2),
    }


@app.post("/recognize")
async def recognize(
    request: Request,
    x_engine_token: str = Header(default=None, alias="X-Engine-Token"),
    request_id: str = Query(default=None),
    timeout_ms: int = Query(default=30000),
):
    """OCR 识别。接收 raw binary PNG body（Content-Type: image/png）。
    request_id 和 timeout_ms 通过 query params 传递。
    """
    verify_token(x_engine_token)

    # request_id 必须原样返回；缺失时拒绝请求，不能生成新 UUID
    if not request_id:
        raise HTTPException(status_code=400, detail="missing_request_id")

    # 模型未 Ready 时不接受业务请求
    if _MODEL_STATE == "Failed":
        raise HTTPException(status_code=503, detail="model_failed")
    if _MODEL_STATE != "Ready":
        raise HTTPException(status_code=503, detail=f"model_not_ready: {_MODEL_STATE}")

    # 确保引擎已初始化（正常情况 Ready 时已初始化，这里做防御性检查）
    engine = _ENGINE
    if engine is None:
        raise HTTPException(status_code=503, detail="engine_not_initialized")

    # 读取 raw binary body
    png_bytes = await request.body()

    # ── 输入资源预算（0.22.6.1）──
    # 与 Rust 侧 input_budget.rs 相同的信任边界；body 上限统一 32 MiB。
    # 在完整解码（PIL → numpy）之前先做 header 级检查，避免无界解码。
    if len(png_bytes) > MAX_BODY_BYTES:
        raise HTTPException(
            status_code=413,
            detail=(
                f"input_too_large: compressed bytes={len(png_bytes)} "
                f"exceeds max={MAX_BODY_BYTES}"
            ),
        )
    if not png_bytes:
        raise HTTPException(status_code=400, detail="image_decode_failed: empty body")

    header_error, (image_width, image_height) = _parse_png_header(png_bytes)
    if header_error is not None:
        raise HTTPException(status_code=400, detail=f"image_decode_failed: {header_error}")
    if image_width > MAX_DIMENSION or image_height > MAX_DIMENSION:
        raise HTTPException(
            status_code=413,
            detail=(
                f"input_too_large: dimensions={image_width}x{image_height} "
                f"exceeds max_side={MAX_DIMENSION}"
            ),
        )
    # checked 预算：width * height * 4（按 RGBA 上界计），乘法显式防溢出
    # （Python int 无溢出，此处保持与 Rust checked arithmetic 相同的边界语义）
    decoded_bytes = image_width * image_height * 4
    if decoded_bytes > MAX_DECODED_BYTES:
        raise HTTPException(
            status_code=413,
            detail=(
                f"input_too_large: decoded={decoded_bytes} bytes "
                f"({image_width}x{image_height}x4) exceeds max={MAX_DECODED_BYTES}"
            ),
        )

    # 执行 OCR（在 worker thread 中，避免阻塞 event loop）
    # PaddleOCR 3.7 predict() 只接受 str(文件路径) 或 numpy.ndarray，不接受 bytes
    import numpy as _np
    from PIL import Image as _PILImage

    try:
        _pil_img = _PILImage.open(io.BytesIO(png_bytes))
        # 屏幕截图 PNG 通常带 alpha 通道（RGBA/4 通道），而 PaddleX 识别预处理
        # 只接受 3 通道输入，4 通道会在 rec 阶段触发无消息的 assert 失败。
        # 解码后统一归一为 RGB（convert 会保留宽高）。
        if _pil_img.mode != "RGB":
            _pil_img = _pil_img.convert("RGB")
        _img_array = _np.array(_pil_img)
    except Exception as e:
        raise HTTPException(status_code=400, detail=f"image_decode_failed: {type(e).__name__}: {e}")

    # 实际 PNG width/height——供 Rust mapper 做 rect 边界校验
    # header 与解码结果不一致说明 PNG 内部损坏，按解码错误处理
    if (_pil_img.width, _pil_img.height) != (image_width, image_height):
        raise HTTPException(
            status_code=400,
            detail=(
                f"image_decode_failed: header size {image_width}x{image_height} "
                f"mismatch decoded {_pil_img.width}x{_pil_img.height}"
            ),
        )
    image_width = _pil_img.width
    image_height = _pil_img.height

    import asyncio
    loop = asyncio.get_event_loop()

    # timeout_ms 已从 query param 获取（默认 30000）

    def _do_ocr():
        return engine.predict(input=_img_array, return_word_box=True)

    try:
        result = await asyncio.wait_for(
            loop.run_in_executor(None, _do_ocr),
            timeout=timeout_ms / 1000.0
        )
    except asyncio.TimeoutError:
        raise HTTPException(status_code=408, detail="timeout_exceeded")
    except Exception as e:
        # PaddleX 内部异常可能 str 为空（如无消息 AssertionError），
        # detail 必须带异常类型才可排查；traceback 打到 stderr 供日志采集。
        traceback.print_exc()
        raise HTTPException(
            status_code=500, detail=f"ocr_failed: {type(e).__name__}: {e}"
        )

    # 映射 PaddleOCR 3.7 predict() 结果到 OcrResult 兼容格式
    lines = []
    words = []
    native_word_boxes = 0
    fallback_word_boxes = 0

    if result:
        for page_result in result:
            try:
                if hasattr(page_result, 'json'):
                    page_data = page_result.json
                    if isinstance(page_data, str):
                        page_data = json.loads(page_data)
                elif hasattr(page_result, '__dict__'):
                    page_data = vars(page_result)
                else:
                    page_data = page_result
            except Exception:
                page_data = page_result

            _extract_results_into(page_data, lines, words, image_width, image_height)

    # 重新统计 native vs fallback
    for w in words:
        if w.get("_native", False):
            native_word_boxes += 1
        else:
            fallback_word_boxes += 1

    return {
        "request_id": request_id,
        "engine": ENGINE_NAME,
        "model_id": _MODEL_ID,
        "model_revision": MODEL_REVISION,
        "image_width": image_width,
        "image_height": image_height,
        "lines": lines,
        "words": [{k: v for k, v in w.items() if k != "_native"} for w in words],
        "elapsed_ms": 0,  # 由调用方计算
        "native_word_boxes": native_word_boxes,
        "fallback_word_boxes": fallback_word_boxes,
    }


def _extract_results_into(page_data, lines, words, image_width, image_height):
    """委托给 ocr_rect.extract_results（生产映射唯一实现）。

    rect 归一化与 line/word 映射语义全部在 ocr_rect.py（纯 stdlib seam），
    本包装只补上 stderr 诊断回调。
    """
    extract_results(
        page_data,
        lines,
        words,
        image_width,
        image_height,
        warn=lambda msg: print(msg, file=sys.stderr, flush=True),
    )


@app.post("/shutdown")
async def shutdown(
    x_engine_token: str = Header(default=None, alias="X-Engine-Token"),
):
    """优雅关闭。"""
    verify_token(x_engine_token)

    print("[INFO] 收到 shutdown 请求，正在关闭...", flush=True)

    if _SERVER is not None:
        _SERVER.should_exit = True
    return {"status": "shutting_down"}


# ── main ──


def main():
    global args, _TOKEN, _ENGINE_ID, _INSTANCE_ID, _SERVER, _ENDPOINT, _MODEL_ID

    parser = argparse.ArgumentParser(description="Blink PP-OCRv6 OCR HTTP server")
    parser.add_argument("--port", type=int, default=9100)
    parser.add_argument("--model", default="tiny", choices=["tiny", "small", "medium"])
    parser.add_argument("--lang", default="ch")
    parser.add_argument("--token", required=True, help="随机认证 token（由 adapter 传入）")
    parser.add_argument("--engine-id", default="paddleocr", help="引擎 ID（由 adapter 传入）")
    parser.add_argument("--instance-id", default=None, help="实例 ID（由 adapter 传入）")
    parser.add_argument("--model-cache", default="./model-cache")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--cpu-threads", type=int, default=2)
    parser.add_argument("--enable-mkldnn", action="store_true")
    args = parser.parse_args()

    if args.cpu_threads < 1:
        parser.error("--cpu-threads 必须 >= 1")

    # 身份参数（从 adapter LaunchContext 传入）
    _ENGINE_ID = args.engine_id
    if args.instance_id:
        _INSTANCE_ID = args.instance_id

    # 设置模型身份
    _MODEL_ID = model_id_for(args.model)

    thread_value = str(args.cpu_threads)
    for name in ("OMP_NUM_THREADS", "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS", "NUMEXPR_NUM_THREADS"):
        os.environ[name] = thread_value

    # 模型缓存环境变量在 import PaddleOCR/PaddleX 之前生效
    # Rust adapter 已设置 PADDLE_PDX_CACHE_HOME 和 PADDLE_PDX_MODEL_SOURCE=BOS
    # Python 端不再设置 PADDLEX_HOME / PADDLE_OCR_HOME（已删除，由 Rust 统一管理）
    # 确保 model_cache 目录存在
    os.makedirs(args.model_cache, exist_ok=True)

    _TOKEN = args.token
    # 身份协议中的 endpoint 使用 canonical authority（host:port），
    # 与 Rust Endpoint::to_string() 以及 FunASR health 回显保持一致。
    # 业务请求使用的 base URL 由宿主侧单独构造，不能混入身份字段。
    _ENDPOINT = f"{args.host}:{args.port}"

    print(f"[INFO] Blink PP-OCRv6 OCR server 启动 (PaddleOCR 3.7 API)", flush=True)
    print(f"[INFO] engine_id={_ENGINE_ID} instance_id={_INSTANCE_ID}", flush=True)
    print(f"[INFO] model={args.model} ocr_version=PP-OCRv6 cpu_threads={args.cpu_threads} mkldnn={args.enable_mkldnn}", flush=True)
    print(f"[INFO] model_cache={args.model_cache}", flush=True)
    print(f"[INFO] model_id={_MODEL_ID} model_revision={MODEL_REVISION}", flush=True)
    print(f"[INFO] listen={args.host}:{args.port}", flush=True)

    import uvicorn

    config = uvicorn.Config(
        app=app,
        host=args.host,
        port=args.port,
        log_level="info",
        access_log=False,
    )
    _SERVER = uvicorn.Server(config)
    try:
        _SERVER.run()
    finally:
        _SERVER = None
        _ENGINE = None
        import gc
        gc.collect()


if __name__ == "__main__":
    main()
