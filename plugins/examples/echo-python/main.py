"""echo 示例脚本插件（Python 版，0.25.20 托管解释器链路验证）。

JSONL stdio 协议：每行一个完整 JSON。
- 收到 {"type":"query","id":...,"query":...} → 回 {"type":"response","id":...,"items":[...]}
- items[0] 把查询文本原样回显，action 为 copy（回车复制原文）

UTF-8 铁则（0.6）：任何读写之前先 reconfigure 三个标准流，
否则 Windows 默认 GBK 会导致中文双向乱码。
"""

import json
import sys

sys.stdin.reconfigure(encoding="utf-8", errors="replace")
sys.stdout.reconfigure(encoding="utf-8", errors="replace", line_buffering=True)
sys.stderr.reconfigure(encoding="utf-8", errors="replace")


def handle_query(req):
    query = req.get("query") or ""
    items = [
        {
            "title": f"echo: {query}" if query else "echo（无输入）",
            "subtitle": "按回车复制原文 · 托管 Python 运行时",
            "score": 0.5,
            "action": {"type": "copy", "text": query},
        }
    ]
    return {"type": "response", "id": req.get("id"), "items": items}


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            print(f"echo-python: 无法解析请求行: {line[:80]}", file=sys.stderr)
            continue
        # cancel / http_response 对 echo 无意义，忽略
        if req.get("type") == "query":
            print(json.dumps(handle_query(req), ensure_ascii=False))


if __name__ == "__main__":
    main()
