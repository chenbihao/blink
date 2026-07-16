#!/usr/bin/env python3
"""
blink_stt_server.py — Blink 自定义统一 STT 服务（0.10.3）

替代官方 funasr-server，同时支持：
- HTTP POST /v1/audio/transcriptions  ← 非流式（SenseVoice），支持热词/ITN
- WS   /ws/stream                     ← 真流式（Paraformer-zh-streaming）
- GET  /health                        ← 健康检查
- GET  /v1/models                     ← 模型列表

HTTP 端点路径和响应格式与官方 funasr-server 完全兼容，
现有 Rust 侧的 LocalSttEngine 和 is_server_ready_http() 无需修改。

模型 lazy load：只有实际收到请求时才加载对应模型，
非流式模式不会加载 streaming 模型（~880MB），反之亦然。

用法:
    python blink_stt_server.py --model SenseVoiceSmall --port 8000 --device cpu
    python blink_stt_server.py --model SenseVoiceSmall --port 8000 --device cpu \
        --use-itn --hotwords /path/to/hotwords.txt \
        --streaming-model paraformer-zh-streaming
"""

import argparse
import io
import json
import os
import sys
import tempfile
import traceback
import wave
import logging
from typing import Dict, Optional

import numpy as np
import uvicorn
from fastapi import FastAPI, UploadFile, File, Form, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse

# ── 全局状态 ──────────────────────────────────────────────────────────────

app = FastAPI(title="Blink STT Server", version="0.10.3")

# 全局模型实例（lazy load）
_nonstream_model = None      # 非流式模型（SenseVoice）
_stream_model = None         # 流式模型（Paraformer-zh-streaming）
_model_lock_nonstream = None  # threading.Lock
_model_lock_stream = None

# 启动参数
_args: Optional[argparse.Namespace] = None

logger = logging.getLogger("blink_stt_server")


def get_args() -> argparse.Namespace:
    global _args
    if _args is None:
        raise RuntimeError("Server not initialized: _args is None")
    return _args


# ── 模型名解析 ────────────────────────────────────────────────────────────

# FunASR 短名 → ModelScope 完整 ID 映射。
# FunASR 1.3.14 的 AutoModel 内置短名解析在某些场景下会失效
#（ModelScope API 返回 404），因此在这里显式映射到完整 ID。
_MODEL_ALIASES = {
    "SenseVoiceSmall": "iic/SenseVoiceSmall",
    "sensevoice": "iic/SenseVoiceSmall",
    "SenseVoice": "iic/SenseVoiceSmall",
    "paraformer-zh-streaming": "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online",
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


# ── 模型类型检测 ────────────────────────────────────────────────────────

def _is_sensevoice(model_name: str) -> bool:
    """判断模型是否为 SenseVoice。

    SenseVoice 内置 VAD + 标点 + ITN + 情感标签，不需要额外子模型。
    Paraformer / SeacoParaformer 没有内置这些功能，需要配置子模型。
    """
    name_lower = model_name.lower()
    return "sensevoice" in name_lower


# ── 模型加载 ──────────────────────────────────────────────────────────

def _load_nonstream_model():
    """加载非流式模型，lazy load。

    模型类型自动适配子模型配置：
    - SenseVoice: 内置 VAD + 标点 + ITN，无需子模型
    - Paraformer / SeacoParaformer: 需配置 vad_model + punc_model
      - vad_model="fsmn-vad": 语音端点检测（~3MB）
      - punc_model="ct-punc": 标点恢复 + ITN（~1.1GB）

    当 funasr_model == streaming_model 时，共用流式模型实例，不重复加载。
    """
    global _nonstream_model, _model_lock_nonstream
    if _nonstream_model is not None:
        return _nonstream_model

    # 如果非流式模型名与流式模型名相同，直接共用流式模型实例
    args = get_args()
    if (args.streaming_model
            and _stream_model is not None
            and args.model == args.streaming_model):
        _nonstream_model = _stream_model
        logger.info(f"非流式模型 {args.model} 与流式模型相同，共用实例")
        return _nonstream_model

    import threading
    if _model_lock_nonstream is None:
        _model_lock_nonstream = threading.Lock()

    with _model_lock_nonstream:
        if _nonstream_model is not None:
            return _nonstream_model

        # 二次检查：流式模型可能在此期间已加载
        if (args.streaming_model
                and _stream_model is not None
                and args.model == args.streaming_model):
            _nonstream_model = _stream_model
            logger.info(f"非流式模型 {args.model} 与流式模型相同，共用实例")
            return _nonstream_model

        logger.info(f"加载非流式模型: {args.model}, device={args.device}")

        from funasr import AutoModel

        resolved_model = _resolve_model_id(args.model)
        kwargs = {
            "model": resolved_model,
            "device": args.device,
            "disable_update": True,  # 跳过更新检查，减少日志噪声
        }

        # Paraformer / SeacoParaformer 需要额外的 VAD 和标点子模型。
        # SenseVoice 内置了这些功能，不需要（也不应该）添加子模型。
        if not _is_sensevoice(args.model):
            kwargs["vad_model"] = "fsmn-vad"
            kwargs["punc_model"] = "ct-punc"
            logger.info(f"检测到非 SenseVoice 模型，自动配置 vad_model=fsmn-vad, punc_model=ct-punc")
        else:
            logger.info(f"检测到 SenseVoice 模型，使用内置 VAD/标点/ITN")

        _nonstream_model = AutoModel(**kwargs)
        logger.info(f"非流式模型 {args.model} 加载完成")
        return _nonstream_model


def _load_stream_model():
    """加载流式模型（Paraformer-zh-streaming），lazy load。"""
    global _stream_model, _model_lock_stream, _nonstream_model
    if _stream_model is not None:
        return _stream_model

    import threading
    if _model_lock_stream is None:
        _model_lock_stream = threading.Lock()

    with _model_lock_stream:
        if _stream_model is not None:
            return _stream_model

        args = get_args()
        streaming_model = args.streaming_model or "paraformer-zh-streaming"
        logger.info(f"加载流式模型: {streaming_model}, device={args.device}")

        from funasr import AutoModel

        resolved_model = _resolve_model_id(streaming_model)
        _stream_model = AutoModel(
            model=resolved_model,
            device=args.device,
            chunk_size=[0, 10, 5],       # 600ms 块
            # streaming 模型不需要单独 VAD（chunk 内自带）
            disable_update=True,
        )
        logger.info(f"流式模型 {streaming_model} 加载完成")

        # 如果非流式模型名相同，自动共用
        if args.model == streaming_model and _nonstream_model is None:
            _nonstream_model = _stream_model
            logger.info(f"非流式模型 {args.model} 与流式模型相同，共用实例")

        return _stream_model


# ── 音频工具 ──────────────────────────────────────────────────────────────

def _wav_bytes_to_numpy(wav_bytes: bytes) -> np.ndarray:
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


def _f32_bytes_to_numpy(data: bytes) -> np.ndarray:
    """将 f32 little-endian PCM 字节转为 numpy 数组。"""
    return np.frombuffer(data, dtype=np.float32)


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
async def health():
    """健康检查。返回模型加载状态。"""
    return JSONResponse({
        "status": "ok",
        "nonstream_model_loaded": _nonstream_model is not None,
        "stream_model_loaded": _stream_model is not None,
    })


@app.get("/v1/models")
async def list_models():
    """模型列表（兼容 OpenAI API 格式）。"""
    args = get_args()
    models = []
    models.append({
        "id": args.model,
        "object": "model",
        "owned_by": "blink",
    })
    if args.streaming_model:
        models.append({
            "id": args.streaming_model,
            "object": "model",
            "owned_by": "blink",
        })
    return {"object": "list", "data": models}


@app.post("/v1/audio/transcriptions")
async def transcribe(
    file: UploadFile = File(...),
    model: str = Form(default=""),
    hotword: Optional[str] = Form(default=None),
    use_itn: Optional[str] = Form(default=None),
):
    """非流式转录（OpenAI 兼容 API）。

    支持 FunASR 增强参数（官方 server 未暴露）：
    - hotword: 热词字符串（每行 "词 权重"）
    - use_itn: ITN 开关 ("true"/"false")
    """
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
        stt_model = _load_nonstream_model()

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

        # ITN（逆文本归一化：“二零二四年” → “2024年”）
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


# ── WebSocket 流式端点 ─────────────────────────────────────────────────────

# Paraformer streaming 模型的 chunk_size=[0, 10, 5] 中，每个 unit = 60ms = 960 samples (16kHz)。
# 每次 generate 调用应收到 current_chunk = 10 * 960 = 9600 samples（600ms）。
# 客户端（cpal/WASAPI）回调每次只给 ~10ms 的小片段，必须在服务端缓冲对齐后再推理，
# 否则模型 forward 近似空转（rtf≈0），无法产出文本。
_STREAM_CHUNK_SAMPLES = 9600  # 10 * 960 = 600ms at 16kHz


@app.websocket("/ws/stream")
async def ws_stream(ws: WebSocket):
    """真流式转录（WebSocket）。

    协议：
    - Client → Server: binary frame = raw f32 PCM (16kHz, mono, little-endian)
    - Server → Client: text frame = JSON {"text": "partial", "is_final": false}
    - Client 发送空帧时，Server 处理剩余缓冲并发送最终结果 {"text": "...", "is_final": true}

    音频缓冲：客户端每次发送 ~10ms 小片段，服务端缓冲至 600ms（9600 samples）
    后再调用 FunASR generate，确保模型能正常推理。
    """
    await ws.accept()

    # 加载流式模型
    try:
        stream_model = _load_stream_model()
    except Exception as e:
        logger.error(f"流式模型加载失败: {e}")
        await ws.send_text(json.dumps({"error": f"model load failed: {e}"}))
        await ws.close()
        return

    cache: Dict = {}  # FunASR streaming 状态
    accumulated_text = ""
    audio_buffer = np.array([], dtype=np.float32)  # 音频缓冲

    def _process_chunk(audio: np.ndarray, is_final: bool) -> str:
        """调用模型推理并返回识别文本。"""
        result = stream_model.generate(
            input=audio,
            cache=cache,
            is_final=is_final,
            chunk_size=[0, 10, 5],
        )
        if isinstance(result, list) and len(result) > 0:
            return result[0].get("text", "")
        elif isinstance(result, dict):
            return result.get("text", "")
        return ""

    def _merge_text(old: str, new: str) -> str:
        """将新识别文本合并到累积文本中。

        FunASR Paraformer streaming 模型可能返回增量文本（只含本 chunk 新识别的字）
        或累积文本（含之前所有 chunk 的文本）。通过检测 new 是否以 old 为前缀
        来自动适应两种行为，避免重复或丢失。
        """
        if not new:
            return old
        if not old:
            return new
        # 如果 new 以 old 开头，说明模型返回的是累积文本，直接替换
        if new.startswith(old):
            return new
        # 否则是增量文本，追加
        return old + new

    try:
        while True:
            data = await ws.receive_bytes()

            # f32 PCM → numpy
            try:
                audio_chunk = _f32_bytes_to_numpy(data)
            except Exception as e:
                logger.warning(f"音频块解析失败: {e}")
                continue

            if len(audio_chunk) == 0:
                # 空帧 = 客户端 finalize 信号
                logger.info(f"收到空帧（finalize 信号），缓冲剩余 {len(audio_buffer)} samples")
                break

            # 追加到缓冲
            audio_buffer = np.concatenate([audio_buffer, audio_chunk])

            # 缓冲达到一个 chunk 大小时，调用模型推理
            while len(audio_buffer) >= _STREAM_CHUNK_SAMPLES:
                chunk_to_process = audio_buffer[:_STREAM_CHUNK_SAMPLES]
                audio_buffer = audio_buffer[_STREAM_CHUNK_SAMPLES:]

                try:
                    text = _process_chunk(chunk_to_process, is_final=False)
                    if text:
                        accumulated_text = _merge_text(accumulated_text, text)
                        await ws.send_text(json.dumps({
                            "text": accumulated_text,
                            "is_final": False,
                        }))
                except Exception as e:
                    logger.warning(f"流式推理失败: {e}")
                    continue

    except WebSocketDisconnect:
        logger.info("WebSocket 客户端断开，发送最终结果")
    except Exception as e:
        logger.error(f"WebSocket 异常: {e}")
    finally:
        # 处理缓冲中的剩余音频（不足一个 chunk），用 is_final=True 触发最终推理
        try:
            if len(audio_buffer) > 0:
                logger.info(f"最终推理：剩余 {len(audio_buffer)} samples ({len(audio_buffer)/16000:.3f}s)")
                final_text = _process_chunk(audio_buffer, is_final=True)
            else:
                # 无剩余音频，用空帧触发 flush
                final_text = _process_chunk(np.zeros(1, dtype=np.float32), is_final=True)

            if final_text:
                accumulated_text = _merge_text(accumulated_text, final_text)

            await ws.send_text(json.dumps({
                "text": accumulated_text,
                "is_final": True,
            }))
        except Exception as e:
            logger.warning(f"发送最终结果失败: {e}")

        try:
            await ws.close()
        except Exception:
            pass


# ── 启动 ──────────────────────────────────────────────────────────────────

from contextlib import asynccontextmanager

@asynccontextmanager
async def lifespan(app_instance):
    """FastAPI lifespan handler（替代废弃的 on_event('startup')）。"""
    args = get_args()
    model_name = args.model
    streaming_name = args.streaming_model
    same_model = bool(streaming_name) and (model_name == streaming_name)

    logger.info("=" * 60)
    logger.info("Blink STT Server v0.10.3")
    if same_model:
        logger.info(f"  模型:       {model_name}（流式+非流式共用）")
    else:
        logger.info(f"  非流式模型: {model_name}")
        logger.info(f"  流式模型:   {streaming_name or '(禁用)'}")
    logger.info(f"  设备:       {args.device}")
    logger.info(f"  端口:       {args.port}")
    logger.info(f"  热词:       {args.hotwords or '(无)'}")
    logger.info(f"  ITN:        {args.use_itn}")

    import asyncio

    # 预加载模型（避免首次请求时才下载，导致长时间等待）
    # 流式模型：**同步预加载**（必须在 server 接受 WebSocket 连接前完成下载和加载，
    #   否则首次 WebSocket 连接会触发模型下载，导致用户等待数十秒且 UI 卡住）
    if streaming_name:
        logger.info("预加载流式模型（同步，首次需下载模型，请耐心等待）...")
        try:
            loop = asyncio.get_event_loop()
            await loop.run_in_executor(None, _load_stream_model)
            logger.info("流式模型预加载完成")
        except Exception as e:
            logger.error(f"流式模型预加载失败: {e}")
            logger.error("流式 STT 端点 /ws/stream 将不可用，非流式端点仍可使用")

    # 非流式模型：当与流式模型相同时，_load_stream_model 已自动共用实例，无需再加载
    if not same_model:
        def _preload_nonstream():
            try:
                _load_nonstream_model()
            except Exception as e:
                logger.warning(f"非流式模型后台预加载失败（将在首次请求时重试）: {e}")
        asyncio.get_event_loop().run_in_executor(None, _preload_nonstream)

    logger.info("=" * 60)

    yield  # server 运行中

    # shutdown（目前无需特殊清理）


app.router.lifespan_context = lifespan


def parse_args():
    parser = argparse.ArgumentParser(
        description="Blink STT Server (FunASR-based, 0.10.3)"
    )
    parser.add_argument(
        "--model", default="SenseVoiceSmall",
        help="非流式模型标识（默认 SenseVoiceSmall）"
    )
    parser.add_argument(
        "--streaming-model", default=None,
        help="流式模型标识（如 paraformer-zh-streaming，None = 不启用流式）"
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
    return parser.parse_args()


def _check_websocket_support():
    """启动前检测 WebSocket 库是否可用。

    uvicorn 的 WebSocket 端点依赖 `websockets` **或** `wsproto` 库（二选一即可）。
    如果只安装了裸 `uvicorn`（非 `uvicorn[standard]`），WebSocket 端点
    `/ws/stream` 会返回 404，流式 STT 将无法工作。

    检测到两者均缺失时打印醒目警告，指导用户修复。
    """
    has_websockets = False
    has_wsproto = False
    try:
        import websockets  # noqa: F401
        has_websockets = True
    except ImportError:
        pass
    try:
        import wsproto  # noqa: F401
        has_wsproto = True
    except ImportError:
        pass

    if not has_websockets and not has_wsproto:
        logger.warning("=" * 60)
        logger.warning("⚠️  WebSocket 库缺失！流式 STT 端点 /ws/stream 将返回 404。")
        logger.warning("    缺失的库: websockets, wsproto（至少需要其一）")
        logger.warning("    修复方法: pip install 'uvicorn[standard]'")
        logger.warning("    或在 Blink 设置页点击「安装环境」重新安装依赖。")
        logger.warning("=" * 60)
        return False
    return True


def main():
    global _args
    _args = parse_args()

    # 配置日志
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
        datefmt="%H:%M:%S",
        stream=sys.stdout,
    )

    # 启动前检测 WebSocket 库（仅警告，不阻止启动——非流式模式仍可用）
    if _args.streaming_model:
        _check_websocket_support()

    # 启动 uvicorn
    uvicorn.run(
        app,
        host="0.0.0.0",
        port=_args.port,
        log_level="info",
    )


if __name__ == "__main__":
    main()
