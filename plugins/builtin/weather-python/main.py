#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Blink Python 插件示例：天气查询（open-meteo，免费无需 key）

JSONL stdio 协议：
  stdin  每行一个 JSON 请求
  stdout 每行一个 JSON 响应
"""

import json
import sys
import urllib.parse
import urllib.request
from typing import Any, Dict, Optional

# 强制 stdin/stdout/stderr 使用 UTF-8 编码
# Windows 下默认 GBK，会导致：
# 1. Rust 发 UTF-8 中文 → Python 按 GBK 读 → 乱码（"广州"→"骞垮窞"）
# 2. Python 发 UTF-8 中文 → Rust 按 UTF-8 读 → 解析失败
sys.stdin.reconfigure(encoding='utf-8', errors='replace')
sys.stdout.reconfigure(encoding='utf-8', errors='replace', line_buffering=True)
sys.stderr.reconfigure(encoding='utf-8', errors='replace')


def _geocode_city(city: str) -> Optional[Dict[str, Any]]:
    """城市名 -> 经纬度"""
    url = f"https://geocoding-api.open-meteo.com/v1/search?name={urllib.parse.quote(city)}&count=1&language=zh"
    try:
        with urllib.request.urlopen(url, timeout=5) as resp:
            data = json.loads(resp.read())
        if data.get("results") and len(data["results"]) > 0:
            return data["results"][0]
        return None
    except Exception as e:
        print(f"[weather-python] geocode failed: {e}", file=sys.stderr, flush=True)
        return None


def _get_weather(lat: float, lon: float, use_fahrenheit: bool = False) -> Optional[Dict[str, Any]]:
    """获取当前天气"""
    temp_unit = "fahrenheit" if use_fahrenheit else "celsius"
    url = f"https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current_weather=true&temperature_unit={temp_unit}"
    try:
        with urllib.request.urlopen(url, timeout=5) as resp:
            return json.loads(resp.read())
    except Exception as e:
        print(f"[weather-python] weather api failed: {e}", file=sys.stderr, flush=True)
        return None


def _weather_code_to_desc(code: int) -> str:
    """WMO 天气代码 -> 中文描述"""
    codes = {
        0: "晴朗",
        1: "晴",
        2: "多云",
        3: "阴",
        45: "雾",
        48: "雾凇",
        51: "小雨",
        53: "中雨",
        55: "大雨",
        61: "小雨",
        63: "中雨",
        65: "大雨",
        66: "冻雨",
        67: "冻雨",
        71: "小雪",
        73: "中雪",
        75: "大雪",
        77: "雪粒",
        80: "阵雨",
        81: "阵雨",
        82: "强阵雨",
        85: "阵雪",
        86: "强阵雪",
        95: "雷阵雨",
        96: "雷阵雨伴冰雹",
        99: "雷阵雨伴强冰雹",
    }
    return codes.get(code, f"天气代码 {code}")


def handle_query(query_id: str, query: str, settings: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """处理查询请求"""
    print(f"[weather-python] 收到查询: id={query_id}, query={query!r}", file=sys.stderr, flush=True)

    settings = settings or {}
    city = query.strip()

    # 无参数时使用默认城市
    if not city:
        city = settings.get("default_city") or ""
    if not city:
        return {
            "id": query_id,
            "items": [],
            "error": {
                "code": "NO_CITY",
                "message": "请输入城市名，或在设置中配置默认城市"
            }
        }

    # 1. 城市 -> 经纬度
    geo = _geocode_city(city)
    if not geo:
        return {
            "id": query_id,
            "items": [],
            "error": {
                "code": "GEOCODE_FAILED",
                "message": f"未找到城市: {city}"
            }
        }

    lat, lon = geo["latitude"], geo["longitude"]
    city_name = geo.get("name", city)
    country = geo.get("country", "")
    admin1 = geo.get("admin1", "")  # 省级行政区

    # 2. 查天气
    use_fahrenheit = settings.get("temperature_unit") == "fahrenheit"
    weather = _get_weather(lat, lon, use_fahrenheit)
    if not weather:
        return {
            "id": query_id,
            "items": [],
            "error": {
                "code": "WEATHER_API_FAILED",
                "message": "天气查询失败，请检查网络连接"
            }
        }

    current = weather.get("current_weather", {})
    temp = current.get("temperature", "N/A")
    windspeed = current.get("windspeed", "N/A")
    winddir = current.get("winddirection", 0)
    weather_code = current.get("weathercode", 0)
    weather_desc = _weather_code_to_desc(weather_code)
    temp_unit = "°F" if use_fahrenheit else "°C"

    # 构造位置描述
    location = city_name
    if admin1:
        location = f"{location}, {admin1}"
    if country:
        location = f"{location}, {country}"

    # 风向文字
    wind_dirs = ["北", "东北", "东", "东南", "南", "西南", "西", "西北"]
    wind_dir_idx = int((winddir + 22.5) / 45) % 8
    wind_dir_name = wind_dirs[wind_dir_idx]

    # 构造结果项
    items = [
        {
            "title": f"{location}: {temp}{temp_unit}, {weather_desc}",
            "subtitle": f"风速 {windspeed} km/h, {wind_dir_name}风",
            "score": 1.0,
            "action": {
                "type": "copy",
                "text": f"{location}: {temp}{temp_unit}, {weather_desc}, 风速 {windspeed} km/h"
            }
        }
    ]

    return {"id": query_id, "items": items}


def main():
    """主循环：读 stdin JSONL，写 stdout JSONL"""
    # 编码和缓冲已在脚本顶部统一配置

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
                # Python 单线程同步实现，无法取消已发送的请求
                # 真实实现可用多线程 + Event 取消
                pass

        except json.JSONDecodeError as e:
            print(f"[weather-python] JSON parse error: {e}", file=sys.stderr, flush=True)
        except Exception as e:
            print(f"[weather-python] Unexpected error: {e}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    main()
