#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Blink 翻译插件：多引擎文本翻译

JSONL stdio 协议：
  stdin  每行一个 JSON 请求
  stdout 每行一个 JSON 响应

支持引擎：
  - 有道智云 (youdao)
  - 百度翻译 (baidu)
  - DeepL (deepl)
"""

import hashlib
import json
import random
import sys
import time
import urllib.parse
import urllib.request
from typing import Any, Dict, List, Optional

# 强制 stdin/stdout/stderr 使用 UTF-8 编码
sys.stdin.reconfigure(encoding='utf-8', errors='replace')
sys.stdout.reconfigure(encoding='utf-8', errors='replace', line_buffering=True)
sys.stderr.reconfigure(encoding='utf-8', errors='replace')

# 语言代码映射（内部统一 → 各引擎）
LANG_MAP = {
    "auto": {"youdao": "auto", "baidu": "auto", "deepl": "auto"},
    "zh": {"youdao": "zh-CHS", "baidu": "zh", "deepl": "ZH"},
    "en": {"youdao": "en", "baidu": "en", "deepl": "EN"},
    "ja": {"youdao": "ja", "baidu": "jp", "deepl": "JA"},
    "ko": {"youdao": "ko", "baidu": "kor", "deepl": "KO"},
}


def _detect_lang(text: str) -> str:
    """简单检测文本语言：中文为主返回 'zh'，否则返回 'en'"""
    chinese_chars = sum(1 for c in text if '一' <= c <= '鿿')
    return "zh" if chinese_chars > len(text) * 0.3 else "en"


def _auto_swap_lang(text: str, target_lang: str) -> str:
    """自动交换目标语言：中文输入→译英文，英文输入→译中文"""
    if target_lang != "auto":
        return target_lang
    detected = _detect_lang(text)
    return "en" if detected == "zh" else "zh"


# ── 有道智云 ──────────────────────────────────────────────────────────────────

def _youdao_translate(text: str, target_lang: str, app_key: str, app_secret: str) -> Optional[str]:
    """有道智云翻译 API"""
    if not app_key or not app_secret:
        return None

    url = "https://openapi.youdao.com/api"
    salt = str(random.randint(10000, 99999))
    curtime = str(int(time.time()))
    sign_str = app_key + text + salt + curtime + app_secret
    sign = hashlib.sha256(sign_str.encode('utf-8')).hexdigest()

    src_lang = "auto"
    tgt_lang = LANG_MAP.get(target_lang, {}).get("youdao", "zh-CHS")

    params = {
        "q": text,
        "from": src_lang,
        "to": tgt_lang,
        "appKey": app_key,
        "salt": salt,
        "sign": sign,
        "signType": "v3",
        "curtime": curtime,
    }

    try:
        data = urllib.parse.urlencode(params).encode('utf-8')
        req = urllib.request.Request(url, data=data, method='POST')
        with urllib.request.urlopen(req, timeout=8) as resp:
            result = json.loads(resp.read())

        if "translation" in result and result["translation"]:
            return result["translation"][0]
        if "errorCode" in result:
            print(f"[translate] youdao error: {result['errorCode']}", file=sys.stderr, flush=True)
        return None
    except Exception as e:
        print(f"[translate] youdao failed: {e}", file=sys.stderr, flush=True)
        return None


# ── 百度翻译 ──────────────────────────────────────────────────────────────────

def _baidu_translate(text: str, target_lang: str, app_id: str, app_key: str) -> Optional[str]:
    """百度翻译 API"""
    if not app_id or not app_key:
        return None

    url = "https://fanyi-api.baidu.com/api/trans/vip/translate"
    salt = str(random.randint(10000, 99999))
    sign_str = app_id + text + salt + app_key
    sign = hashlib.md5(sign_str.encode('utf-8')).hexdigest()

    src_lang = "auto"
    tgt_lang = LANG_MAP.get(target_lang, {}).get("baidu", "zh")

    params = {
        "q": text,
        "from": src_lang,
        "to": tgt_lang,
        "appid": app_id,
        "salt": salt,
        "sign": sign,
    }

    try:
        query_str = urllib.parse.urlencode(params)
        req_url = f"{url}?{query_str}"
        with urllib.request.urlopen(req_url, timeout=8) as resp:
            result = json.loads(resp.read())

        if "trans_result" in result and result["trans_result"]:
            return "\n".join(item["dst"] for item in result["trans_result"])
        if "error_code" in result:
            print(f"[translate] baidu error: {result['error_code']}", file=sys.stderr, flush=True)
        return None
    except Exception as e:
        print(f"[translate] baidu failed: {e}", file=sys.stderr, flush=True)
        return None


# ── DeepL ─────────────────────────────────────────────────────────────────────

def _deepl_translate(text: str, target_lang: str, api_key: str) -> Optional[str]:
    """DeepL API"""
    if not api_key:
        return None

    # DeepL Free API
    url = "https://api-free.deepl.com/v2/translate"
    tgt_lang = LANG_MAP.get(target_lang, {}).get("deepl", "ZH")

    params = {
        "text": text,
        "target_lang": tgt_lang,
        "auth_key": api_key,
    }

    try:
        data = urllib.parse.urlencode(params).encode('utf-8')
        req = urllib.request.Request(url, data=data, method='POST')
        with urllib.request.urlopen(req, timeout=8) as resp:
            result = json.loads(resp.read())

        if "translations" in result and result["translations"]:
            return result["translations"][0]["text"]
        return None
    except Exception as e:
        print(f"[translate] deepl failed: {e}", file=sys.stderr, flush=True)
        return None


# ── 翻译调度 ──────────────────────────────────────────────────────────────────

def _translate(text: str, target_lang: str, engine: str, settings: Dict[str, Any]) -> Optional[str]:
    """调度翻译引擎"""
    if engine == "youdao":
        return _youdao_translate(
            text, target_lang,
            settings.get("youdao_app_key", ""),
            settings.get("youdao_app_secret", "")
        )
    elif engine == "baidu":
        return _baidu_translate(
            text, target_lang,
            settings.get("baidu_app_id", ""),
            settings.get("baidu_app_key", "")
        )
    elif engine == "deepl":
        return _deepl_translate(
            text, target_lang,
            settings.get("deepl_api_key", "")
        )
    return None


def _try_translate(text: str, target_lang: str, engine: str, settings: Dict[str, Any]) -> Optional[str]:
    """尝试翻译，失败则尝试其他引擎"""
    # 先用指定引擎
    result = _translate(text, target_lang, engine, settings)
    if result:
        return result

    # 失败时尝试其他引擎（按优先级）
    fallback_order = ["youdao", "baidu", "deepl"]
    for fallback in fallback_order:
        if fallback == engine:
            continue
        result = _translate(text, target_lang, fallback, settings)
        if result:
            return result

    return None


# ── 请求处理 ──────────────────────────────────────────────────────────────────

def handle_query(query_id: str, query: str, settings: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """处理翻译查询"""
    print(f"[translate] 收到查询: id={query_id}, query={query!r}", file=sys.stderr, flush=True)

    settings = settings or {}
    engine = settings.get("default_engine", "youdao")
    target_lang = settings.get("target_lang", "zh")

    text = query.strip()

    # 无参数时返回提示
    if not text:
        return {
            "id": query_id,
            "items": [
                {
                    "title": "输入文本开始翻译",
                    "subtitle": f"当前引擎: {engine}，目标语言: {target_lang}",
                    "score": 1.0,
                    "action": {"type": "none"}
                }
            ]
        }

    # 自动交换语言
    target_lang = _auto_swap_lang(text, target_lang)

    # 翻译
    result = _try_translate(text, target_lang, engine, settings)

    if not result:
        return {
            "id": query_id,
            "items": [],
            "error": {
                "code": "TRANSLATE_FAILED",
                "message": "翻译失败，请检查 API 配置或网络连接"
            }
        }

    # 构造结果
    lang_display = {"zh": "中文", "en": "英文", "ja": "日文", "ko": "韩文"}
    target_display = lang_display.get(target_lang, target_lang)

    items = [
        {
            "title": f"📝 {result}",
            "subtitle": f"原文: {text[:50]}{'...' if len(text) > 50 else ''} | 目标: {target_display}",
            "score": 1.0,
            "action": {
                "type": "copy",
                "text": result
            }
        }
    ]

    # 添加快捷动作
    items.append({
        "title": "📋 复制译文",
        "subtitle": "将翻译结果复制到剪贴板",
        "score": 0.9,
        "action": {
            "type": "copy",
            "text": result
        }
    })

    items.append({
        "title": "📋 复制原文",
        "subtitle": "将原文复制到剪贴板",
        "score": 0.8,
        "action": {
            "type": "copy",
            "text": text
        }
    })

    return {"id": query_id, "items": items}


# ── 主循环 ────────────────────────────────────────────────────────────────────

def main():
    """主循环：读 stdin JSONL，写 stdout JSONL"""
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        try:
            req = json.loads(line)
            req_type = req.get("type")

            if req_type == "query":
                query_id = req.get("id", "")
                query = req.get("query", "")
                settings = req.get("settings")
                resp = handle_query(query_id, query, settings)
                print(json.dumps(resp, ensure_ascii=False), flush=True)

            elif req_type == "cancel":
                pass

        except json.JSONDecodeError as e:
            print(f"[translate] JSON parse error: {e}", file=sys.stderr, flush=True)
        except Exception as e:
            print(f"[translate] Unexpected error: {e}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    main()
