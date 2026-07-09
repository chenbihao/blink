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
  - 阿里机器翻译 (ali)
  - 腾讯云机器翻译 (tencent)
"""

import base64
import datetime
import hashlib
import hmac
import json
import random
import re
import sys
import time
import uuid
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


def _preprocess_code_identifiers(text: str) -> str:
    """拆分程序员命名风格，让翻译 API 能正确理解。

    处理：
      - snake_case / SCREAMING_SNAKE → 空格分隔
      - camelCase / PascalCase → 空格分隔（连续大写缩写保持整体，如 HTTP、URL）
      - kebab-case → 空格分隔

    单个单词和中文不受影响（正则不匹配 = 原样返回）。
    """
    # 下划线 / 连字符 → 空格
    text = re.sub(r'[_\-]+', ' ', text)
    # camelCase / PascalCase 拆分：
    #   大写+小写前断开（HTTPSConnection → HTTPS Connection）
    #   小写+大写前断开（getUserName → get UserName）
    text = re.sub(r'([A-Z]+)([A-Z][a-z])', r'\1 \2', text)
    text = re.sub(r'([a-z])([A-Z])', r'\1 \2', text)
    return text.strip()


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


# ── 阿里机器翻译 ──────────────────────────────────────────────────────────────

def _ali_translate(text: str, target_lang: str, access_key_id: str, access_key_secret: str) -> Optional[str]:
    """阿里机器翻译 API (HMAC-SHA1 签名)"""
    if not access_key_id or not access_key_secret:
        return None

    url = "https://mt.aliyuncs.com/"

    # 语言代码映射
    lang_map = {"zh": "zh", "en": "en", "ja": "ja", "ko": "ko"}
    src_lang = "auto"
    tgt_lang = lang_map.get(target_lang, "zh")

    # 公共参数
    params = {
        "Format": "JSON",
        "Version": "2018-10-12",
        "AccessKeyId": access_key_id,
        "SignatureMethod": "HMAC-SHA1",
        "Timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "SignatureVersion": "1.0",
        "SignatureNonce": str(uuid.uuid4()),
        "Action": "TranslateGeneral",
        "SourceLanguage": src_lang,
        "TargetLanguage": tgt_lang,
        "SourceText": text,
        "FormatType": "text",
        "Scene": "general",
    }

    try:
        # 1. 按字母排序并 URL 编码
        sorted_params = sorted(params.items())
        canonicalized = "&".join(
            [f"{urllib.parse.quote(k, safe='')}={urllib.parse.quote(str(v), safe='')}"
             for k, v in sorted_params]
        )

        # 2. 构造签名字符串
        string_to_sign = f"POST&%2F&{urllib.parse.quote(canonicalized, safe='')}"

        # 3. 计算签名 (HMAC-SHA1)
        signing_key = access_key_secret + "&"
        signature = base64.b64encode(
            hmac.new(signing_key.encode(), string_to_sign.encode(), hashlib.sha1).digest()
        ).decode()

        params["Signature"] = signature

        # 4. 发送请求
        data = urllib.parse.urlencode(params).encode('utf-8')
        req = urllib.request.Request(url, data=data, method='POST')
        with urllib.request.urlopen(req, timeout=8) as resp:
            result = json.loads(resp.read())

        print(f"[translate] ali response: {json.dumps(result, ensure_ascii=False)}", file=sys.stderr, flush=True)

        if "Data" in result and "Translated" in result["Data"]:
            return result["Data"]["Translated"]
        # 打印详细错误信息
        if "Code" in result:
            print(f"[translate] ali error code: {result.get('Code')}, message: {result.get('Message', 'N/A')}, request_id: {result.get('RequestId', 'N/A')}", file=sys.stderr, flush=True)
        elif "Message" in result:
            print(f"[translate] ali error: {result['Message']}", file=sys.stderr, flush=True)
        return None
    except urllib.error.HTTPError as e:
        body = e.read().decode('utf-8', errors='replace') if e.fp else ''
        print(f"[translate] ali HTTP error: {e.code} {e.reason}, body: {body[:500]}", file=sys.stderr, flush=True)
        return None
    except Exception as e:
        print(f"[translate] ali failed: {type(e).__name__}: {e}", file=sys.stderr, flush=True)
        return None


# ── 腾讯云机器翻译 ────────────────────────────────────────────────────────────

def _tencent_translate(text: str, target_lang: str, secret_id: str, secret_key: str) -> Optional[str]:
    """腾讯云机器翻译 API (TC3-HMAC-SHA256 签名)"""
    if not secret_id or not secret_key:
        return None

    service = "tmt"
    host = "tmt.tencentcloudapi.com"
    endpoint = "https://tmt.tencentcloudapi.com"
    action = "TextTranslate"
    version = "2018-03-21"
    region = "ap-guangzhou"

    # 语言代码映射
    lang_map = {"zh": "zh", "en": "en", "ja": "ja", "ko": "ko"}
    src_lang = "auto"
    tgt_lang = lang_map.get(target_lang, "zh")

    # 请求体
    payload = json.dumps({
        "SourceText": text,
        "Source": src_lang,
        "Target": tgt_lang,
        "ProjectId": 0
    })

    try:
        timestamp = int(time.time())
        date = datetime.datetime.utcfromtimestamp(timestamp).strftime('%Y-%m-%d')

        # Step 1: 规范请求
        http_request_method = "POST"
        canonical_uri = "/"
        canonical_querystring = ""
        content_type = "application/json; charset=utf-8"
        canonical_headers = f"content-type:{content_type}\nhost:{host}\nx-tc-action:{action.lower()}\n"
        signed_headers = "content-type;host;x-tc-action"
        hashed_payload = hashlib.sha256(payload.encode("utf-8")).hexdigest()
        canonical_request = (f"{http_request_method}\n{canonical_uri}\n{canonical_querystring}\n"
                            f"{canonical_headers}\n{signed_headers}\n{hashed_payload}")

        # Step 2: 拼接待签名字符串
        algorithm = "TC3-HMAC-SHA256"
        credential_scope = f"{date}/{service}/tc3_request"
        hashed_canonical_request = hashlib.sha256(canonical_request.encode("utf-8")).hexdigest()
        string_to_sign = f"{algorithm}\n{timestamp}\n{credential_scope}\n{hashed_canonical_request}"

        # Step 3: 计算签名
        secret_date = hmac.new(("TC3" + secret_key).encode("utf-8"), date.encode("utf-8"), hashlib.sha256).digest()
        secret_service = hmac.new(secret_date, service.encode("utf-8"), hashlib.sha256).digest()
        secret_signing = hmac.new(secret_service, "tc3_request".encode("utf-8"), hashlib.sha256).digest()
        signature = hmac.new(secret_signing, string_to_sign.encode("utf-8"), hashlib.sha256).hexdigest()

        # Step 4: 拼接 Authorization
        authorization = f"{algorithm} Credential={secret_id}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"

        # Step 5: 发送请求
        headers = {
            "Authorization": authorization,
            "Content-Type": content_type,
            "Host": host,
            "X-TC-Action": action,
            "X-TC-Timestamp": str(timestamp),
            "X-TC-Version": version,
            "X-TC-Region": region,
        }

        req = urllib.request.Request(endpoint, data=payload.encode('utf-8'), headers=headers, method='POST')
        with urllib.request.urlopen(req, timeout=8) as resp:
            result = json.loads(resp.read())

        if "Response" in result and "TargetText" in result["Response"]:
            return result["Response"]["TargetText"]
        if "Error" in result.get("Response", {}):
            print(f"[translate] tencent error: {result['Response']['Error']}", file=sys.stderr, flush=True)
        return None
    except Exception as e:
        print(f"[translate] tencent failed: {e}", file=sys.stderr, flush=True)
        return None


# ── 翻译调度 ──────────────────────────────────────────────────────────────────

# 引擎显示名称
ENGINE_NAMES = {
    "youdao": "有道智云",
    "baidu": "百度翻译",
    "deepl": "DeepL",
    "ali": "阿里翻译",
    "tencent": "腾讯翻译",
}

def _translate(text: str, target_lang: str, engine: str, settings: Dict[str, Any]) -> Optional[str]:
    """调度翻译引擎"""
    engine_name = ENGINE_NAMES.get(engine, engine)
    print(f"[translate] 使用引擎: {engine_name} ({engine}), 目标语言: {target_lang}", file=sys.stderr, flush=True)

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
    elif engine == "ali":
        return _ali_translate(
            text, target_lang,
            settings.get("ali_access_key_id", ""),
            settings.get("ali_access_key_secret", "")
        )
    elif engine == "tencent":
        return _tencent_translate(
            text, target_lang,
            settings.get("tencent_secret_id", ""),
            settings.get("tencent_secret_key", "")
        )
    return None


def _try_translate(text: str, target_lang: str, engine: str, settings: Dict[str, Any]) -> Optional[str]:
    """尝试翻译，失败则尝试其他引擎"""
    # 先用指定引擎
    result = _translate(text, target_lang, engine, settings)
    if result:
        return result

    # 从配置读取降级顺序，默认按优先级
    fallback_value = settings.get("fallback_order", ["tencent", "ali", "baidu", "youdao", "deepl"])
    # 兼容字符串和列表两种格式
    if isinstance(fallback_value, str):
        fallback_order = [e.strip() for e in fallback_value.split(",")]
    else:
        fallback_order = list(fallback_value)  # 列表格式
    # 过滤有效引擎
    valid_engines = set(ENGINE_NAMES.keys())
    fallback_order = [e for e in fallback_order if e in valid_engines]

    # 失败时按配置顺序尝试其他引擎
    for fallback in fallback_order:
        if fallback == engine:
            continue
        fallback_name = ENGINE_NAMES.get(fallback, fallback)
        print(f"[translate] 主引擎失败，降级到: {fallback_name} ({fallback})", file=sys.stderr, flush=True)
        result = _translate(text, target_lang, fallback, settings)
        if result:
            return result

    return None


# ── 请求处理 ──────────────────────────────────────────────────────────────────

def handle_query(query_id: str, query: str, settings: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """处理翻译查询"""
    settings = settings or {}
    engine = settings.get("default_engine", "youdao")
    target_lang = settings.get("target_lang", "zh")
    engine_name = ENGINE_NAMES.get(engine, engine)

    print(f"[translate] 收到查询: id={query_id}, query={query!r}, 引擎={engine_name}, 目标语言={target_lang}", file=sys.stderr, flush=True)

    text = query.strip()

    # 空参数已由框架层 empty_arg_hint 拦截（不会走到这里）；如果因兼容性/降级仍走到，
    # 返回空 items 让框架清占位即可，避免与 manifest 里的 hint 文案重复。
    if not text:
        return {"id": query_id, "items": []}

    # 程序员命名风格预处理：snake_case / camelCase / SCREAMING_SNAKE → 空格分隔
    # 在语言检测前执行，帮助识别为英文而非乱码
    original_text = text  # 保留原始输入用于结果显示/复制
    text = _preprocess_code_identifiers(text)

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
            "subtitle": f"按 Enter 复制译文 | 原文: {original_text[:50]}{'...' if len(original_text) > 50 else ''}",
            "score": 1.0,
            "action": {
                "type": "copy",
                "text": result
            }
        },
    ]

    # 预处理改变了文本 → 额外提供拆分后的版本供复制
    if text != original_text:
        items.append({
            "title": f"🔤 {text}",
            "subtitle": "按 Enter 复制拆分后的命名 | 来自命名风格预处理",
            "score": 0.9,
            "action": {
                "type": "copy",
                "text": text
            }
        })

    items.append({
        "title": f"📄 {original_text[:60]}{'...' if len(original_text) > 60 else ''}",
        "subtitle": "按 Enter 复制原文",
        "score": 0.8,
        "action": {
            "type": "copy",
            "text": original_text
        }
    })

    return {"id": query_id, "items": items}


# ── 0.9.3 tool-call 处理 ────────────────────────────────────────────────────

def handle_tool_call(tool_call_id: str, tool_name: str, arguments: Dict[str, Any],
                     settings: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """处理 AI tool-call 请求（0.9.3）

    返回格式与 PluginResponse 统一：{"id": ..., "items": [...], "error": ...}
    items 复用 PluginItem 结构：{title, subtitle, score, action}
    """
    settings = settings or {}
    engine = settings.get("default_engine", "youdao")

    if tool_name == "translate":
        text = arguments.get("text", "").strip()
        if not text:
            return {
                "id": tool_call_id,
                "items": [],
                "error": {"code": "MISSING_ARG", "message": "缺少 text 参数"}
            }

        target_lang = arguments.get("target_lang", "")
        if not target_lang or target_lang == "auto":
            target_lang = settings.get("target_lang", "zh")
        target_lang = _auto_swap_lang(text, target_lang)

        print(f"[translate] tool_call: id={tool_call_id}, text={text!r}, target={target_lang}",
              file=sys.stderr, flush=True)

        # 翻译
        result = _try_translate(text, target_lang, engine, settings)

        if not result:
            return {
                "id": tool_call_id,
                "items": [],
                "error": {"code": "TRANSLATE_FAILED", "message": "翻译失败，请检查 API 配置或网络连接"}
            }

        lang_display = {"zh": "中文", "en": "英文", "ja": "日文", "ko": "韩文"}
        target_display = lang_display.get(target_lang, target_lang)

        # 复用 PluginItem 格式，与 handle_query 统一
        items = [
            {
                "title": f"📝 {result}",
                "subtitle": f"翻译自: {text[:50]}{'...' if len(text) > 50 else ''} → {target_display}",
                "score": 1.0,
                "action": {"type": "copy", "text": result}
            },
            {
                "title": f"📄 {text[:60]}{'...' if len(text) > 60 else ''}",
                "subtitle": "按 Enter 复制原文",
                "score": 0.8,
                "action": {"type": "copy", "text": text}
            }
        ]

        return {"id": tool_call_id, "items": items}

    return {
        "id": tool_call_id,
        "items": [],
        "error": {"code": "UNKNOWN_TOOL", "message": f"未知 tool: {tool_name}"}
    }


# ── 主循环 ────────────────────────────────────────────────────────────────────

def main():
    """主循环：读 stdin JSONL，写 stdout JSONL"""
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        query_id = ""
        try:
            req = json.loads(line)
            req_type = req.get("type")

            if req_type == "query":
                query_id = req.get("id", "")
                query = req.get("query", "")
                settings = req.get("settings")
                resp = handle_query(query_id, query, settings)
                print(json.dumps(resp, ensure_ascii=False), flush=True)

            elif req_type == "tool_call":
                # 0.9.3: AI tool-call 请求
                tool_call_id = req.get("id", "")
                tool_name = req.get("tool_name", "")
                arguments = req.get("arguments", {})
                settings = req.get("settings")
                resp = handle_tool_call(tool_call_id, tool_name, arguments, settings)
                # 包装成 PluginUpstreamMessage::ToolResult 格式
                wrapped = {"type": "tool_result", **resp}
                print(json.dumps(wrapped, ensure_ascii=False), flush=True)

            elif req_type == "cancel":
                pass

        except json.JSONDecodeError as e:
            print(f"[translate] JSON parse error: {e}", file=sys.stderr, flush=True)
        except Exception as e:
            print(f"[translate] Unexpected error: {e}", file=sys.stderr, flush=True)
            # 异常时仍向 stdout 返回错误响应，避免前端等到超时
            if query_id:
                resp = {
                    "id": query_id,
                    "items": [],
                    "error": {
                        "code": "INTERNAL_ERROR",
                        "message": f"插件内部错误：{e}"
                    }
                }
                print(json.dumps(resp, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
