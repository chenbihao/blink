#!/usr/bin/env python3
"""PP-OCRv6 thin Blink HTTP wrapper — 拓扑 A。

使用 PaddleOCR 3.7 API（predict()），不依赖 PaddleX serving。
启动后在后台初始化模型；/health 正确呈现 NotLoaded/Loading/Ready/Failed。
模型加载只发生一次；并发请求不重复初始化。
OCR 同步 CPU 调用在 worker thread 执行，不阻塞 FastAPI event loop。

协议草案见 protocol.md。所有 health 和业务请求要求随机 token。

用法：
    python server_thin.py --port 9100 --model small --token <random> --model-cache ./model-cache
"""

import argparse
import base64
import io
import os
import secrets
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
_ENGINE_LOCK = threading.Lock()  # 保护模型初始化的 single-flight
_TOKEN = None
_INSTANCE_ID = str(uuid.uuid4())
_START_TIME = time.time()
_MODEL_STATE = "NotLoaded"  # NotLoaded | Loading | Ready | Failed
_MODEL_REVISION = None
_MODEL_ID = "PP-OCRv6"
_init_start_time = None
_http_reachable_time = None
_model_ready_time = None
_SERVER = None

# ── 模型档位 → 官方模型名映射 ──

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

# 全局 args（在 main 中赋值）
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
    """初始化 PaddleOCR 3.7 引擎。single-flight：只执行一次。"""
    global _ENGINE, _init_start_time, _model_ready_time

    with _ENGINE_LOCK:
        if _ENGINE is not None:
            return _ENGINE

        _init_start_time = time.perf_counter()
        _set_model_state("Loading")

        try:
            from paddleocr import PaddleOCR  # type: ignore

            model_names = MODEL_MAP.get(args.model, MODEL_MAP["small"])

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
            revision = _get_model_revision()
            _set_model_state("Ready", revision)

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
        except Exception as e:
            # 错误已在 init_engine 中记录
            pass

    t = threading.Thread(target=_bg_init, daemon=True)
    t.start()
    return t


# ── HTTP 服务 ──

try:
    from fastapi import FastAPI, Header, HTTPException, Request
    from fastapi.responses import JSONResponse
    from pydantic import BaseModel
except ImportError:
    print("[FATAL] FastAPI 未安装", file=sys.stderr)
    sys.exit(1)

app = FastAPI(title="PP-OCRv6 Thin Wrapper")


class RecognizeRequest(BaseModel):
    image: str  # base64-encoded PNG
    request_id: str | None = None
    timeout_ms: int | None = None


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
    init_engine_background()


@app.get("/health")
async def health(x_engine_token: str = Header(default=None, alias="X-Engine-Token")):
    """健康检查。"""
    if not _TOKEN:
        raise HTTPException(status_code=503, detail="service_not_ready")
    if x_engine_token != _TOKEN:
        raise HTTPException(status_code=401, detail="invalid_token")

    return {
        "protocol_version": "0.2.0",
        "engine_id": "paddleocr-ppocrv6",
        "instance_id": _INSTANCE_ID,
        "service_state": "healthy",
        "model_state": _MODEL_STATE,
        "model_id": _MODEL_ID,
        "model_revision": _MODEL_REVISION,
        "uptime_seconds": round(time.time() - _START_TIME, 2),
    }


@app.post("/recognize")
async def recognize(
    req: RecognizeRequest,
    x_engine_token: str = Header(default=None, alias="X-Engine-Token"),
):
    """OCR 识别。模型未 Ready 时返回 503。"""
    verify_token(x_engine_token)

    # 模型未 Ready 时不接受业务请求
    if _MODEL_STATE == "Failed":
        raise HTTPException(status_code=503, detail="model_failed")
    if _MODEL_STATE != "Ready":
        raise HTTPException(status_code=503, detail=f"model_not_ready: {_MODEL_STATE}")

    # 确保引擎已初始化（正常情况 Ready 时已初始化，这里做防御性检查）
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
    # PaddleOCR 3.7 predict() 只接受 str(文件路径) 或 numpy.ndarray，不接受 bytes
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

    timeout_ms = req.timeout_ms or 60000  # 默认 60s（大图可能需要更多时间）

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
        raise HTTPException(status_code=500, detail=f"ocr_failed: {e}")

    # 映射 PaddleOCR 3.7 predict() 结果到 OcrResult 兼容格式
    # predict() 返回一个 generator/list of OCRResult 对象
    lines = []
    words = []
    word_idx = 0
    native_word_boxes = 0
    fallback_word_boxes = 0

    if result:
        # predict() 返回 list，每个元素对应一张图
        for page_result in result:
            # OCRResult 对象有 .json 属性包含结构化数据
            try:
                if hasattr(page_result, 'json'):
                    page_data = page_result.json
                    if isinstance(page_data, str):
                        import json
                        page_data = json.loads(page_data)
                elif hasattr(page_result, '__dict__'):
                    page_data = vars(page_result)
                else:
                    page_data = page_result
            except Exception:
                page_data = page_result

            # PaddleOCR 3.7 predict() 返回的 OCRResult 结构：
            # res = { "rec_texts": [...], "rec_scores": [...], "dt_polys": [...], "dt_scores": [...] }
            # 当 return_word_box=True 时额外有 "word_boxes" 字段
            _extract_results(page_data, lines, words, 
                            lambda: (native_word_boxes, fallback_word_boxes))

    # 重新统计 native vs fallback
    for w in words:
        if w.get("_native", False):
            native_word_boxes += 1
        else:
            fallback_word_boxes += 1

    return {
        "request_id": req.request_id or str(uuid.uuid4()),
        "lines": lines,
        "words": [{k: v for k, v in w.items() if k != "_native"} for w in words],
        "elapsed_ms": 0,  # 由调用方计算
        "engine": "ppocrv6-thin",
        "model": args.model,
        "native_word_boxes": native_word_boxes,
        "fallback_word_boxes": fallback_word_boxes,
    }


def _extract_results(page_data, lines, words, get_counts):
    """从 PaddleOCR 3.7 predict() 结果提取 line/word 数据。

    PaddleOCR 3.7 OCRResult.json 结构（实测）：
    {
        "res": {
            "rec_texts": ["line1", "line2"],
            "rec_scores": [0.95, 0.88],
            "dt_polys": [[[x1,y1],[x2,y2],[x3,y3],[x4,y4]], ...],
            "rec_boxes": [[x1,y1,x2,y2], ...],
            "text_word_boxes": [[[x1,y1,x2,y2],...], ...],  # 每行的 word 矩形列表
            "text_word": [["w1","w2",...], ...],            # 每行的 word 文本列表
        }
    }
    """
    if not page_data:
        return

    # PaddleOCR 3.7 数据嵌套在 "res" 字段下
    if isinstance(page_data, dict) and "res" in page_data:
        res = page_data["res"]
    elif isinstance(page_data, dict):
        res = page_data
    else:
        return

    if not isinstance(res, dict):
        return

    rec_texts = res.get("rec_texts", [])
    rec_scores = res.get("rec_scores", [])
    dt_polys = res.get("dt_polys", [])
    rec_boxes = res.get("rec_boxes", [])
    text_word_boxes = res.get("text_word_boxes", [])
    text_word = res.get("text_word", [])

    # 构建 line 和 word 数据
    for i, text in enumerate(rec_texts):
        line_idx = len(lines)

        # Line rect from dt_polys (4-point polygon) or rec_boxes (x1,y1,x2,y2)
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

        # Word 级数据：text_word_boxes[i] = [[x1,y1,x2,y2], ...], text_word[i] = ["w1", "w2", ...]
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
        else:
            # Fallback: 用文本拆分作为 word（line rect 复制）
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

        lines.append({
            "text": text,
            "rect": line_rect,
            "word_indices": line_word_indices,
            "confidence": round(conf, 4),
        })


@app.post("/shutdown")
async def shutdown(
    x_engine_token: str = Header(default=None, alias="X-Engine-Token"),
):
    """优雅关闭。"""
    verify_token(x_engine_token)

    print("[INFO] 收到 shutdown 请求，正在关闭...", flush=True)

    # sys.exit() 在 Timer 子线程里只会退出该线程。通知 Uvicorn 主循环退出，
    # 模型资源在 server.run() 返回后的 main-thread finally 中释放。
    if _SERVER is not None:
        _SERVER.should_exit = True
    return {"status": "shutting_down"}


# ── main ──


def main():
    global args, _TOKEN, _ENGINE, _SERVER

    parser = argparse.ArgumentParser(description="PP-OCRv6 thin HTTP wrapper")
    parser.add_argument("--port", type=int, default=9100)
    parser.add_argument("--model", default="small", choices=["tiny", "small", "medium"])
    parser.add_argument("--lang", default="ch")
    parser.add_argument("--token", required=True, help="随机认证 token")
    parser.add_argument("--model-cache", default="./model-cache")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--cpu-threads", type=int, default=2)
    parser.add_argument("--enable-mkldnn", action="store_true")
    args = parser.parse_args()

    if args.cpu_threads < 1:
        parser.error("--cpu-threads 必须 >= 1")

    thread_value = str(args.cpu_threads)
    for name in ("OMP_NUM_THREADS", "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS", "NUMEXPR_NUM_THREADS"):
        os.environ[name] = thread_value

    _TOKEN = args.token
    os.makedirs(args.model_cache, exist_ok=True)

    print(f"[INFO] PP-OCRv6 thin wrapper 启动 (PaddleOCR 3.7 API)", flush=True)
    print(f"[INFO] instance_id={_INSTANCE_ID}", flush=True)
    print(f"[INFO] model={args.model} ocr_version=PP-OCRv6 cpu_threads={args.cpu_threads} mkldnn={args.enable_mkldnn}", flush=True)
    print(f"[INFO] model_cache={args.model_cache}", flush=True)
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
