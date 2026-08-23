#!/usr/bin/env python3
"""PP-OCRv6 PaddleX basic serving 适配 — 拓扑 B。

使用 PaddleX 的 pipeline serving API 启动 PP-OCRv6 服务。
与拓扑 A（thin wrapper）使用相同的输入集、模型、预热规则和输出契约。

**禁止 fallback 到 PaddleOCR thin wrapper。** 如果 PaddleX 不可用，
本拓扑必须明确 FAIL，不得伪装为 thin wrapper 后仍标记 paddlex。

用法：
    python server_paddlex.py --port 9101 --model small --token <random> --model-cache ./model-cache
"""

import argparse
import base64
import os
import sys
import threading
import time
import uuid

# UTF-8 安全（spec-backend §九）
sys.stdin.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace", line_buffering=True)
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

# ── 全局状态 ──

_ENGINE = None
_ENGINE_LOCK = threading.Lock()
_TOKEN = None
_INSTANCE_ID = str(uuid.uuid4())
_START_TIME = time.time()
_MODEL_STATE = "NotLoaded"  # NotLoaded | Loading | Ready | Failed
_MODEL_REVISION = None
_MODEL_ID = "PP-OCRv6"
_init_start_time = None
_http_reachable_time = None
_model_ready_time = None
_PADDLEX_AVAILABLE = False
_PADDLEX_VERSION = None
_SERVER = None

MODEL_MAP = {
    "tiny": {"det": "PP-OCRv6_tiny_det", "rec": "PP-OCRv6_tiny_rec"},
    "small": {"det": "PP-OCRv6_small_det", "rec": "PP-OCRv6_small_rec"},
    "medium": {"det": "PP-OCRv6_medium_det", "rec": "PP-OCRv6_medium_rec"},
}

# 全局 args
args = None


def _set_model_state(state, revision=None):
    global _MODEL_STATE, _MODEL_REVISION
    _MODEL_STATE = state
    if revision is not None:
        _MODEL_REVISION = revision
    print(f"[STATE] model_state={state} revision={_MODEL_REVISION}", flush=True)


def _get_model_revision():
    """扫描模型缓存目录，记录实际下载的模型文件列表和大小。"""
    try:
        cache_dir = args.model_cache
        files_info = []
        if os.path.exists(cache_dir):
            for root, dirs, files in os.walk(cache_dir):
                for f in files:
                    fp = os.path.join(root, f)
                    try:
                        size = os.path.getsize(fp)
                        files_info.append(f"{f}:{size}B")
                    except OSError:
                        pass
        return f"cache_files:{len(files_info)}"
    except Exception:
        return "unknown"


def init_engine():
    """初始化 PaddleX pipeline。single-flight：只执行一次。

    **禁止 fallback 到 PaddleOCR。** 如果 PaddleX 不可用，直接 Failed。
    """
    global _ENGINE, _init_start_time, _model_ready_time
    global _PADDLEX_AVAILABLE, _PADDLEX_VERSION

    with _ENGINE_LOCK:
        if _ENGINE is not None:
            return _ENGINE

        _init_start_time = time.perf_counter()
        _set_model_state("Loading")

        try:
            # 尝试导入 PaddleX
            try:
                import paddlex  # type: ignore
                _PADDLEX_AVAILABLE = True
                _PADDLEX_VERSION = getattr(paddlex, "__version__", "unknown")
                print(f"[INFO] PaddleX version: {_PADDLEX_VERSION}", flush=True)
            except ImportError:
                _PADDLEX_AVAILABLE = False
                _PADDLEX_VERSION = None
                _set_model_state("Failed")
                print("[ERROR] PaddleX 未安装，拓扑 B 不可用。禁止 fallback 到 PaddleOCR thin wrapper。", file=sys.stderr, flush=True)
                return None

            if not _PADDLEX_AVAILABLE:
                _set_model_state("Failed")
                return None

            # PaddleX 的 OCR pipeline 使用方式：
            # from paddlex import create_pipeline
            # pipeline = create_pipeline(pipeline="OCR")
            #
            # PaddleX 3.x 使用 create_pipeline API
            try:
                from paddlex import create_pipeline  # type: ignore
                from paddlex.inference.pipelines import load_pipeline_config  # type: ignore

                # 默认 OCR.yaml 固定 medium 且启用文档矫正/方向模型，既不尊重
                # --model，也不等价于截图 OCR。显式改写同一份官方配置。
                model_names = MODEL_MAP[args.model]
                pipeline_config = load_pipeline_config("OCR")
                pipeline_config["use_doc_preprocessor"] = False
                pipeline_config["use_textline_orientation"] = False
                pipeline_config["SubModules"]["TextDetection"]["model_name"] = model_names["det"]
                pipeline_config["SubModules"]["TextRecognition"]["model_name"] = model_names["rec"]

                _ENGINE = create_pipeline(
                    config=pipeline_config,
                    device="cpu",
                    engine_config={
                        "cpu_threads": args.cpu_threads,
                        "run_mode": "mkldnn" if args.enable_mkldnn else "paddle",
                    },
                )
            except ImportError as e:
                _set_model_state("Failed")
                print(f"[ERROR] PaddleX create_pipeline API 不可用: {e}", file=sys.stderr, flush=True)
                return None

            _model_ready_time = time.perf_counter()
            revision = _get_model_revision()
            _set_model_state("Ready", revision)

            load_ms = round((_model_ready_time - _init_start_time) * 1000, 2)
            print(f"[INFO] PaddleX 模型加载完成: {args.model} ({load_ms}ms)", flush=True)

        except Exception as e:
            _set_model_state("Failed")
            print(f"[ERROR] PaddleX 模型加载失败: {e}", file=sys.stderr, flush=True)
            # 不 raise，让 Failed 状态保持

    return _ENGINE


def init_engine_background():
    """在后台线程初始化模型，不阻塞服务启动。"""
    def _bg_init():
        try:
            init_engine()
        except Exception:
            pass

    t = threading.Thread(target=_bg_init, daemon=True)
    t.start()
    return t


# ── HTTP 服务 ──

try:
    from fastapi import FastAPI, Header, HTTPException
    from pydantic import BaseModel
except ImportError:
    print("[FATAL] FastAPI 未安装", file=sys.stderr)
    sys.exit(1)

app = FastAPI(title="PP-OCRv6 PaddleX Serving")


class RecognizeRequest(BaseModel):
    image: str
    request_id: str | None = None
    timeout_ms: int | None = None


def verify_token(x_engine_token: str = Header(default=None, alias="X-Engine-Token")):
    if not _TOKEN:
        raise HTTPException(status_code=503, detail="service_not_ready")
    if x_engine_token != _TOKEN:
        raise HTTPException(status_code=401, detail="invalid_token")


@app.on_event("startup")
async def startup_event():
    global _http_reachable_time
    _http_reachable_time = time.perf_counter()
    print(f"[INFO] HTTP reachable at {_http_reachable_time:.2f}s", flush=True)
    init_engine_background()


@app.get("/health")
async def health(x_engine_token: str = Header(default=None, alias="X-Engine-Token")):
    if not _TOKEN:
        raise HTTPException(status_code=503, detail="service_not_ready")
    if x_engine_token != _TOKEN:
        raise HTTPException(status_code=401, detail="invalid_token")

    return {
        "protocol_version": "0.2.0",
        "engine_id": "paddlex-ppocrv6",
        "instance_id": _INSTANCE_ID,
        "service_state": "healthy",
        "model_state": _MODEL_STATE,
        "model_id": _MODEL_ID,
        "model_revision": _MODEL_REVISION,
        "paddlex_available": _PADDLEX_AVAILABLE,
        "paddlex_version": _PADDLEX_VERSION,
        "uptime_seconds": round(time.time() - _START_TIME, 2),
    }


@app.post("/recognize")
async def recognize(
    req: RecognizeRequest,
    x_engine_token: str = Header(default=None, alias="X-Engine-Token"),
):
    """OCR 识别。模型未 Ready 时返回 503。"""
    verify_token(x_engine_token)

    if _MODEL_STATE == "Failed":
        raise HTTPException(status_code=503, detail="model_failed")
    if _MODEL_STATE != "Ready":
        raise HTTPException(status_code=503, detail=f"model_not_ready: {_MODEL_STATE}")

    engine = _ENGINE
    if engine is None:
        raise HTTPException(status_code=503, detail="engine_not_initialized")

    # 解码图片
    try:
        png_bytes = base64.b64decode(req.image)
    except Exception:
        raise HTTPException(status_code=400, detail="invalid_base64_image")

    if len(png_bytes) > 20 * 1024 * 1024:
        raise HTTPException(status_code=413, detail="image_too_large")

    # 执行 OCR（在 worker thread 中，避免阻塞 event loop）
    # PaddleOCR 3.7 predict() 只接受 str(文件路径) 或 numpy.ndarray
    import io as _io
    import numpy as _np
    from PIL import Image as _PILImage

    try:
        _pil_img = _PILImage.open(_io.BytesIO(png_bytes))
        _img_array = _np.array(_pil_img)
    except Exception as e:
        raise HTTPException(status_code=400, detail=f"image_decode_failed: {e}")

    import asyncio
    loop = asyncio.get_event_loop()

    timeout_ms = req.timeout_ms or 60000  # 默认 60s

    def _do_ocr():
        result = engine.predict(input=_img_array)
        return result

    try:
        result = await asyncio.wait_for(
            loop.run_in_executor(None, _do_ocr),
            timeout=timeout_ms / 1000.0
        )
    except asyncio.TimeoutError:
        raise HTTPException(status_code=408, detail="timeout_exceeded")
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"ocr_failed: {e}")

    # 映射 PaddleX pipeline predict 结果到 OcrResult 兼容格式
    # PaddleX OCR pipeline 返回的结果结构与 PaddleOCR predict() 类似
    lines = []
    words = []
    native_word_boxes = 0
    fallback_word_boxes = 0

    if result:
        for page_result in result:
            try:
                if hasattr(page_result, "json"):
                    page_data = page_result.json
                    if isinstance(page_data, str):
                        import json
                        page_data = json.loads(page_data)
                elif hasattr(page_result, "__dict__"):
                    page_data = vars(page_result)
                else:
                    page_data = page_result
            except Exception:
                page_data = page_result

            # PaddleOCR 3.7 OCRResult.json 结构（实测）：
            # { "res": { "rec_texts": [...], "rec_scores": [...], "dt_polys": [...],
            #            "rec_boxes": [...], "text_word_boxes": [...], "text_word": [...] } }
            if isinstance(page_data, dict) and "res" in page_data:
                res = page_data["res"]
            elif isinstance(page_data, dict):
                res = page_data
            else:
                continue

            if not isinstance(res, dict):
                continue

            rec_texts = res.get("rec_texts", [])
            rec_scores = res.get("rec_scores", [])
            dt_polys = res.get("dt_polys", [])
            rec_boxes = res.get("rec_boxes", [])
            text_word_boxes = res.get("text_word_boxes", [])
            text_word = res.get("text_word", [])

            line_idx = len(lines)

            # 构建 line 和 word 数据
            for i, text in enumerate(rec_texts):
                # Line rect from dt_polys or rec_boxes
                if i < len(dt_polys) and dt_polys[i]:
                    box = dt_polys[i]
                    if box and len(box) >= 4:
                        xs = [p[0] for p in box]
                        ys = [p[1] for p in box]
                        line_rect = {
                            "x": round(min(xs)),
                            "y": round(min(ys)),
                            "w": round(max(xs) - min(xs)),
                            "h": round(max(ys) - min(ys)),
                        }
                    else:
                        line_rect = {"x": 0, "y": 0, "w": 0, "h": 0}
                elif i < len(rec_boxes) and rec_boxes[i]:
                    box = rec_boxes[i]
                    line_rect = {
                        "x": round(box[0]),
                        "y": round(box[1]),
                        "w": round(box[2] - box[0]),
                        "h": round(box[3] - box[1]),
                    }
                else:
                    line_rect = {"x": 0, "y": 0, "w": 0, "h": 0}

                conf = rec_scores[i] if i < len(rec_scores) else 0.0

                # Word 级数据
                line_word_indices = []
                if i < len(text_word_boxes) and i < len(text_word):
                    word_boxes_i = text_word_boxes[i]
                    word_texts_i = text_word[i]
                    for j, w_box in enumerate(word_boxes_i):
                        w_text = word_texts_i[j] if j < len(word_texts_i) else ""
                        if isinstance(w_box, (list, tuple)) and len(w_box) >= 4:
                            word_rect = {
                                "x": round(w_box[0]),
                                "y": round(w_box[1]),
                                "w": round(w_box[2] - w_box[0]),
                                "h": round(w_box[3] - w_box[1]),
                            }
                        else:
                            word_rect = {"x": 0, "y": 0, "w": 0, "h": 0}

                        words.append({
                            "text": w_text,
                            "rect": word_rect,
                            "line_index": line_idx,
                            "_native": True,
                        })
                        line_word_indices.append(len(words) - 1)
                        native_word_boxes += 1
                else:
                    # Fallback
                    word_texts = text.split() if text else []
                    if not word_texts and text:
                        word_texts = [text]
                    for wt in word_texts:
                        words.append({
                            "text": wt,
                            "rect": line_rect,
                            "line_index": line_idx,
                            "_native": False,
                        })
                        line_word_indices.append(len(words) - 1)
                        fallback_word_boxes += 1

                lines.append({
                    "text": text,
                    "rect": line_rect,
                    "word_indices": line_word_indices,
                    "confidence": round(conf, 4),
                })

    return {
        "request_id": req.request_id or str(uuid.uuid4()),
        "lines": lines,
        "words": [{k: v for k, v in w.items() if k != "_native"} for w in words],
        "elapsed_ms": 0,
        "engine": "ppocrv6-paddlex",
        "model": args.model,
        "paddlex_version": _PADDLEX_VERSION,
        "native_word_boxes": native_word_boxes,
        "fallback_word_boxes": fallback_word_boxes,
    }


@app.post("/shutdown")
async def shutdown(
    x_engine_token: str = Header(default=None, alias="X-Engine-Token"),
):
    verify_token(x_engine_token)
    if _SERVER is not None:
        _SERVER.should_exit = True
    return {"status": "shutting_down"}


def main():
    global args, _TOKEN, _ENGINE, _SERVER

    parser = argparse.ArgumentParser(description="PP-OCRv6 PaddleX serving")
    parser.add_argument("--port", type=int, default=9101)
    parser.add_argument("--model", default="small", choices=["tiny", "small", "medium"])
    parser.add_argument("--lang", default="ch")
    parser.add_argument("--token", required=True)
    parser.add_argument("--model-cache", default="./model-cache")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--cpu-threads", type=int, default=2)
    parser.add_argument("--enable-mkldnn", action="store_true")
    args = parser.parse_args()

    if args.cpu_threads < 1:
        parser.error("--cpu-threads 必须 >= 1")

    thread_value = str(args.cpu_threads)
    for name in ("OMP_NUM_THREADS", "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS", "NUMEXPR_NUM_THREADS", "PADDLE_PDX_CPU_NUM_THREADS"):
        os.environ[name] = thread_value

    _TOKEN = args.token
    os.makedirs(args.model_cache, exist_ok=True)

    print(f"[INFO] PP-OCRv6 PaddleX serving 启动", flush=True)
    print(f"[INFO] instance_id={_INSTANCE_ID}", flush=True)
    print(f"[INFO] model={args.model} lang={args.lang} cpu_threads={args.cpu_threads} mkldnn={args.enable_mkldnn}", flush=True)
    print(f"[INFO] listen={args.host}:{args.port}", flush=True)
    print(f"[INFO] PaddleX serving: 禁止 fallback 到 thin wrapper", flush=True)

    import uvicorn

    config = uvicorn.Config(app=app, host=args.host, port=args.port, log_level="info", access_log=False)
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
