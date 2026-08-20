#!/usr/bin/env python3
"""
fetch-model-context-windows.py — 从 LiteLLM 仓库精选主流模型条目生成精简 JSON。

数据源：https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json
输出：resources/model_context_windows.json

精选范围：OpenAI / Anthropic / Gemini / DeepSeek / GLM / Qwen / Moonshot / Kimi
每条只留 prefix → max_input_tokens。

用法：python xtask/scripts/fetch-model-context-windows.py [workspace_root]
workspace_root 默认为脚本上溯两级。
"""

import json
import os
import sys
import urllib.request

LITELLM_URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"

# 精选模型前缀模式——只保留主流模型的条目
# (model_id_prefix, context_window_override)
# context_window_override = None 时从 LiteLLM 取 max_input_tokens
SELECTED_MODELS = [
    # OpenAI
    ("gpt-5", None),
    ("gpt-4o-mini", None),
    ("gpt-4o", None),
    ("gpt-4.1-mini", None),
    ("gpt-4.1", None),
    ("gpt-4-turbo", None),
    ("gpt-4", None),
    ("gpt-3.5", None),
    ("o3-mini", None),
    ("o3", None),
    ("o1-mini", None),
    ("o1", None),
    # Anthropic
    ("claude-opus-4", None),
    ("claude-sonnet-4", None),
    ("claude-3-7", None),
    ("claude-3-5-sonnet", None),
    ("claude-3-5-haiku", None),
    ("claude-3-opus", None),
    ("claude-3-sonnet", None),
    ("claude-3-haiku", None),
    # Gemini
    ("gemini-2.5-pro", None),
    ("gemini-2.5-flash", None),
    ("gemini-2.0-flash", None),
    ("gemini-1.5-pro", None),
    ("gemini-1.5-flash", None),
    ("gemini-pro", None),
    # DeepSeek
    ("deepseek-chat", None),
    ("deepseek-reasoner", None),
    ("deepseek-coder", None),
    # GLM
    ("glm-4-plus", None),
    ("glm-4-air", None),
    ("glm-4-long", None),
    ("glm-4", None),
    ("glm-3-turbo", None),
    # Qwen
    ("qwen-max", None),
    ("qwen-plus", None),
    ("qwen-turbo", None),
    ("qwen2.5-72b", None),
    ("qwen2.5-32b", None),
    ("qwen2.5-7b", None),
    ("qwen2-72b", None),
    # Moonshot
    ("moonshot-v1-8k", 8192),
    ("moonshot-v1-32k", 32768),
    ("moonshot-v1-128k", 131072),
    # Kimi
    ("kimi-latest", None),
    ("kimi-k2", None),
]


def main():
    workspace_root = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    )

    print(f"Fetching LiteLLM model data from {LITELLM_URL} ...")
    try:
        req = urllib.request.Request(LITELLM_URL, headers={"User-Agent": "blink-xtask/1.0"})
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except Exception as e:
        print(f"ERROR: Failed to fetch LiteLLM data: {e}")
        sys.exit(1)

    # LiteLLM 的 JSON 是 {model_id: {max_input_tokens: ..., ...}, ...}
    # 但 sample_for_model_names key 格式是 "prompt_tokens" etc.
    # 实际格式是 top-level 为 JSON object，每个 key 是 model name
    models = []
    for prefix, override in SELECTED_MODELS:
        # 在 LiteLLM 数据中查找以 prefix 开头的模型
        context_window = override
        if context_window is None:
            # 查找以 prefix 开头的第一个条目
            for model_name, model_info in data.items():
                if model_name.startswith(prefix) and isinstance(model_info, dict):
                    cw = model_info.get("max_input_tokens") or model_info.get("input_context_length")
                    if cw and isinstance(cw, (int, float)):
                        context_window = int(cw)
                        break

        if context_window is None:
            # 回退：尝试精确匹配
            if prefix in data and isinstance(data[prefix], dict):
                cw = data[prefix].get("max_input_tokens") or data[prefix].get("input_context_length")
                if cw and isinstance(cw, (int, float)):
                    context_window = int(cw)

        if context_window is None:
            print(f"  WARN: No context window found for prefix '{prefix}', skipping")
            continue

        models.append({"prefix": prefix, "context_window": context_window})
        print(f"  {prefix}: {context_window}")

    # 按前缀长度降序排列（确保更长的前缀先匹配）
    models.sort(key=lambda m: len(m["prefix"]), reverse=True)

    output = {
        "_comment": "Blink 内置精简模型目录 — 从 LiteLLM model_prices_and_context_window.json 精选主流条目。构建期 include_str! 嵌入，运行时零文件依赖。前缀匹配取更长者（gpt-4o-mini 优先于 gpt-4）。0.21.21。",
        "models": models,
    }

    output_path = os.path.join(workspace_root, "resources", "model_context_windows.json")
    os.makedirs(os.path.dirname(output_path), exist_ok=True)
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(output, f, indent=2, ensure_ascii=False)
        f.write("\n")

    print(f"\nGenerated: {output_path} ({len(models)} models)")


if __name__ == "__main__":
    main()
