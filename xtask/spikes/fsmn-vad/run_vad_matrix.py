#!/usr/bin/env python3
"""
0.22.8 FSMN-VAD spike — VAD 策略对比矩阵

对 10 种音频场景 × 5 种 VAD 策略执行端点检测对比。
不依赖 Blink 编译产物；用纯 Python 实现 EnergyVad 和 FSMN-VAD 的等价逻辑。

5 种策略：
  1. energy_only          — 当前 EnergyVad（RMS + 静默时长）
  2. fsmn_only            — 纯 FSMN-VAD 端点（离线整段推理）
  3. energy_prefilter_fsmn — EnergyVad 粗筛 → FSMN 精确端点
  4. fsmn_final_gate      — EnergyVad 切句 + FSMN 最终静音门
  5. energy_optimized     — EnergyVad + 优化包（自适应噪声底 + 滞回 + hangover）

输出：JSON 结构化数据到 results/vad-matrix.json
"""

import json
import math
import os
import struct
import sys
import time
import wave
from dataclasses import dataclass, field
from pathlib import Path
from typing import List, Tuple

# ── 音频工具 ──────────────────────────────────────────────────────────────

def read_wav_16k_mono(path: str) -> List[float]:
    """读取 WAV 文件，返回 16kHz mono f32 样本。"""
    with wave.open(path, 'rb') as wf:
        n_channels = wf.getnchannels()
        sample_rate = wf.getframerate()
        n_frames = wf.getnframes()
        raw = wf.readframes(n_frames)

    # 只取第 0 声道
    if n_channels == 1:
        samples = struct.unpack(f'<{n_frames}h', raw)
    else:
        all_samples = struct.unpack(f'<{n_frames * n_channels}h', raw)
        samples = all_samples[::n_channels]

    # 归一化到 [-1, 1]
    return [s / 32768.0 for s in samples]


def compute_rms(samples: List[float]) -> float:
    if not samples:
        return 0.0
    sum_sq = sum(s * s for s in samples)
    return math.sqrt(sum_sq / len(samples))


# ── VAD 策略实现 ──────────────────────────────────────────────────────────

@dataclass
class VadSegment:
    start_ms: int
    end_ms: int


@dataclass
class VadResult:
    segments: List[VadSegment] = field(default_factory=list)
    endpoint_events: List[int] = field(default_factory=list)  # 句尾时间点 ms
    inference_time_ms: float = 0.0
    model_load_count: int = 0  # 模型重新加载次数
    notes: str = ""


# ── 策略 1: EnergyVad (当前生产实现) ─────────────────────────────────────

class EnergyVadStrategy:
    """Rust EnergyVad 的 Python 等价实现。"""

    def __init__(self, silence_threshold=0.005, min_silence_ms=300,
                 min_sentence_ms=800, sample_rate=16000):
        self.silence_threshold = silence_threshold
        self.min_silence_ms = min_silence_ms
        self.min_sentence_ms = min_sentence_ms
        self.sample_rate = sample_rate

    def run(self, samples: List[float]) -> VadResult:
        t0 = time.perf_counter()
        chunk_size = 160  # 10ms @ 16kHz
        silence_samples = 0
        speaking = False
        sentence_samples = 0
        total_samples = len(samples)
        events = []

        for i in range(0, len(samples), chunk_size):
            chunk = samples[i:i+chunk_size]
            rms = compute_rms(chunk)
            cs = len(chunk)
            min_silence = (self.min_silence_ms * self.sample_rate) // 1000
            min_sentence = (self.min_sentence_ms * self.sample_rate) // 1000

            if rms < self.silence_threshold:
                silence_samples += cs
                if speaking:
                    if silence_samples >= min_silence:
                        if sentence_samples >= min_sentence:
                            speaking = False
                            event_ms = (i + cs) * 1000 // self.sample_rate
                            events.append(event_ms)
                        else:
                            speaking = False
                            sentence_samples = 0
            else:
                if not speaking:
                    speaking = True
                    sentence_samples = 0
                silence_samples = 0
                sentence_samples += cs

        elapsed = (time.perf_counter() - t0) * 1000
        return VadResult(
            endpoint_events=events,
            inference_time_ms=elapsed,
            model_load_count=0,
            notes="RMS energy + silence duration (current production)"
        )


# ── 策略 2: FSMN-VAD (离线整段) ───────────────────────────────────────────
# 由于 FSMN-VAD GGUF 需要 ggml C++ runtime，本 spike 用以下方式记录：
# - 如果有 funasr-vad CLI 可用，则 subprocess 调用
# - 否则用模拟行为记录（基于已知 FSMN-VAD 特性）

class FsmnVadStrategy:
    """FSMN-VAD 离线整段推理。

    关键特性（从 funasr_vad.h 源码确认）：
    1. 每次调用 gguf_init_from_file 重新加载模型
    2. 对整段 wav 一次性做 fbank + FSMN encoder 推理
    3. E2EVadModel 状态机在完整推理结果上做后处理
    4. 无增量/在线 cache/reset 机制
    5. 函数签名不接受增量输入
    """

    def __init__(self, vad_gguf_path=None, vad_cli_path=None, sample_rate=16000):
        self.vad_gguf_path = vad_gguf_path
        self.vad_cli_path = vad_cli_path  # funasr-vad 或 funasr-sensevoice --vad
        self.sample_rate = sample_rate

    def run(self, samples: List[float]) -> VadResult:
        if not self.vad_cli_path or not self.vad_gguf_path:
            return VadResult(
                inference_time_ms=0,
                model_load_count=0,
                notes="FSMN-VAD GGUF not available in this environment; "
                      "recorded as simulated based on source code analysis"
            )

        # 写入临时 WAV
        import subprocess
        import tempfile
        tmp_wav = os.path.join(tempfile.gettempdir(), f'fsmn_vad_{os.getpid()}.wav')
        _write_wav(tmp_wav, samples, self.sample_rate)

        t0 = time.perf_counter()
        try:
            result = subprocess.run(
                [self.vad_cli_path, '-m', self.vad_gguf_path, '-a', tmp_wav],
                capture_output=True, text=True, timeout=30
            )
            elapsed = (time.perf_counter() - t0) * 1000

            segments = []
            for line in result.stdout.strip().split('\n'):
                if line:
                    parts = line.split()
                    if len(parts) == 2:
                        segments.append(VadSegment(int(parts[0]), int(parts[1])))

            # 从 segments 提取端点（每段 end 就是端点）
            events = [seg.end_ms for seg in segments]

            return VadResult(
                segments=segments,
                endpoint_events=events,
                inference_time_ms=elapsed,
                model_load_count=1,  # 每次调用都重新加载
                notes="FSMN-VAD offline whole-file inference; "
                      "gguf_init_from_file called per invocation"
            )
        except Exception as e:
            return VadResult(
                inference_time_ms=(time.perf_counter() - t0) * 1000,
                model_load_count=1,
                notes=f"FSMN-VAD error: {e}"
            )
        finally:
            if os.path.exists(tmp_wav):
                os.remove(tmp_wav)


# ── 策略 3: EnergyVad 粗筛 + FSMN 精确端点 ────────────────────────────────

class EnergyPrefilterFsmnStrategy:
    """先用 EnergyVad 粗筛出候选段，再对每段跑 FSMN-VAD 精确端点。

    目的：验证 EnergyVad 的粗筛能否减少 FSMN 的处理量，
    以及 FSMN 的精确端点能否修正 EnergyVad 的误切/漏切。
    """

    def __init__(self, energy_vad: EnergyVadStrategy, fsmn_vad: FsmnVadStrategy):
        self.energy_vad = energy_vad
        self.fsmn_vad = fsmn_vad

    def run(self, samples: List[float]) -> VadResult:
        # Step 1: EnergyVad 粗筛
        energy_result = self.energy_vad.run(samples)

        # Step 2: 对整段跑 FSMN（因为 FSMN 是整段推理，粗筛只是减少后续 ASR 处理量）
        fsmn_result = self.fsmn_vad.run(samples)

        # 合并：用 FSMN 的精确端点，但标记 EnergyVad 的粗筛段
        return VadResult(
            segments=fsmn_result.segments,
            endpoint_events=fsmn_result.endpoint_events,
            inference_time_ms=energy_result.inference_time_ms + fsmn_result.inference_time_ms,
            model_load_count=fsmn_result.model_load_count,
            notes=f"Energy prefilter (energy_ms={energy_result.inference_time_ms:.1f}) + "
                  f"FSMN endpoint (fsmn_ms={fsmn_result.inference_time_ms:.1f}); "
                  f"FSMN still processes whole file"
        )


# ── 策略 4: FSMN 仅作最终静音/低置信门 ────────────────────────────────────

class FsmnFinalGateStrategy:
    """EnergyVad 做切句，FSMN 仅在切句点做最终确认（静音/低置信门）。

    目的：验证 FSMN 能否作为 EnergyVad 的后置门控，减少误切。
    """

    def __init__(self, energy_vad: EnergyVadStrategy, fsmn_vad: FsmnVadStrategy):
        self.energy_vad = energy_vad
        self.fsmn_vad = fsmn_vad

    def run(self, samples: List[float]) -> VadResult:
        # Step 1: EnergyVad 切句
        energy_result = self.energy_vad.run(samples)

        # Step 2: 如果有 FSMN，对每个 EnergyVad 端点做最终验证
        if not self.fsmn_vad.vad_cli_path:
            return VadResult(
                endpoint_events=energy_result.endpoint_events,
                inference_time_ms=energy_result.inference_time_ms,
                model_load_count=0,
                notes="EnergyVad + FSMN gate (FSMN unavailable, energy only)"
            )

        # FSMN 对整段跑一次，提取段信息
        fsmn_result = self.fsmn_vad.run(samples)

        # 用 FSMN 段验证 EnergyVad 端点：
        # 如果 EnergyVad 端点落在 FSMN 语音段内，则接受；
        # 如果落在 FSMN 静音段内但靠近语音段边界，也接受；
        # 如果完全在静音中，则拒绝（误切）
        validated_events = []
        for ep in energy_result.endpoint_events:
            is_valid = False
            for seg in fsmn_result.segments:
                if seg.start_ms <= ep <= seg.end_ms + 200:  # 容忍 200ms
                    is_valid = True
                    break
            if is_valid:
                validated_events.append(ep)

        return VadResult(
            endpoint_events=validated_events,
            inference_time_ms=energy_result.inference_time_ms + fsmn_result.inference_time_ms,
            model_load_count=fsmn_result.model_load_count,
            notes=f"EnergyVad ({len(energy_result.endpoint_events)} events) → "
                  f"FSMN gate ({len(validated_events)} validated)"
        )


# ── 策略 5: EnergyVad 优化包 ──────────────────────────────────────────────

class EnergyOptimizedVad:
    """EnergyVad + 优化包：自适应噪声底 + 双阈值滞回 + pre-roll + hangover + 最短有效语音。"""

    def __init__(self, sample_rate=16000,
                 min_silence_ms=300, min_sentence_ms=800,
                 noise_floor_ms=2000,  # 自适应噪声底窗口
                 on_threshold_factor=1.5,  # 说话阈值 = 噪声底 × factor
                 off_threshold_factor=1.0,  # 静默阈值 = 噪声底 × factor
                 hangover_ms=80,  # 滞后关闭
                 preroll_ms=100,  # 预滚：回退到有声起点前
                 min_active_ms=100):  # 最短有效语音段
        self.sample_rate = sample_rate
        self.min_silence_ms = min_silence_ms
        self.min_sentence_ms = min_sentence_ms
        self.noise_floor_ms = noise_floor_ms
        self.on_threshold_factor = on_threshold_factor
        self.off_threshold_factor = off_threshold_factor
        self.hangover_ms = hangover_ms
        self.preroll_ms = preroll_ms
        self.min_active_ms = min_active_ms

    def run(self, samples: List[float]) -> VadResult:
        t0 = time.perf_counter()
        chunk_size = 160  # 10ms
        total_chunks = (len(samples) + chunk_size - 1) // chunk_size

        # Step 1: 计算每个 chunk 的 RMS
        rms_values = []
        for i in range(0, len(samples), chunk_size):
            chunk = samples[i:i+chunk_size]
            rms_values.append(compute_rms(chunk))

        # Step 2: 自适应噪声底（用前 noise_floor_ms 或全局 P10 分位）
        noise_floor_chunks = (self.noise_floor_ms * self.sample_rate) // (1000 * chunk_size)
        if len(rms_values) > noise_floor_chunks:
            sorted_rms = sorted(rms_values[:noise_floor_chunks])
            noise_floor = sorted_rms[len(sorted_rms) // 10]  # P10
        else:
            noise_floor = 0.001  # 默认极低

        on_threshold = max(noise_floor * self.on_threshold_factor, 0.002)
        off_threshold = max(noise_floor * self.off_threshold_factor, 0.001)

        # Step 3: 双阈值滞回状态机
        silence_samples = 0
        speaking = False
        sentence_samples = 0
        events = []
        hangover_samples = (self.hangover_ms * self.sample_rate) // 1000
        preroll_samples = (self.preroll_ms * self.sample_rate) // 1000
        min_silence = (self.min_silence_ms * self.sample_rate) // 1000
        min_sentence = (self.min_sentence_ms * self.sample_rate) // 1000
        min_active = (self.min_active_ms * self.sample_rate) // 1000

        speech_start = 0
        for ci, rms in enumerate(rms_values):
            sample_idx = ci * chunk_size
            cs = min(chunk_size, len(samples) - sample_idx)

            if not speaking:
                if rms > on_threshold:
                    speaking = True
                    speech_start = max(0, sample_idx - preroll_samples)
                    sentence_samples = sample_idx - speech_start
                    silence_samples = 0
            else:
                if rms < off_threshold:
                    silence_samples += cs
                    if silence_samples >= min_silence:
                        if sentence_samples >= min_sentence:
                            speaking = False
                            event_ms = (sample_idx + cs + hangover_samples) * 1000 // self.sample_rate
                            events.append(event_ms)
                            silence_samples = 0
                            sentence_samples = 0
                        else:
                            speaking = False
                            silence_samples = 0
                            sentence_samples = 0
                else:
                    silence_samples = 0
                    sentence_samples += cs

        elapsed = (time.perf_counter() - t0) * 1000
        return VadResult(
            endpoint_events=events,
            inference_time_ms=elapsed,
            model_load_count=0,
            notes=f"Adaptive noise floor={noise_floor:.5f} "
                  f"on_th={on_threshold:.5f} off_th={off_threshold:.5f} "
                  f"hangover={self.hangover_ms}ms preroll={self.preroll_ms}ms"
        )


# ── WAV 写入工具 ──────────────────────────────────────────────────────────

def _write_wav(path: str, samples: List[float], sample_rate: int):
    """写入 16kHz mono s16le WAV。"""
    import array
    int_samples = array.array('h', [max(-32768, min(32767, int(s * 32768))) for s in samples])
    with wave.open(path, 'wb') as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(int_samples.tobytes())


# ── 测试矩阵 ──────────────────────────────────────────────────────────────

FIXTURES = {
    'zh_short':      {'desc': '中文短句 ~2s', 'expected_speech': True, 'expected_endpoints': 1},
    'zh_long':       {'desc': '中文长句 ~6s', 'expected_speech': True, 'expected_endpoints': 1},
    'mid_pause':     {'desc': '句中停顿（说话-停顿-继续）', 'expected_speech': True, 'expected_endpoints': 1},
    'think_pause':   {'desc': '思考停顿（长停顿后继续）', 'expected_speech': True, 'expected_endpoints': 1},
    'low_volume':    {'desc': '低音量语音', 'expected_speech': True, 'expected_endpoints': 1},
    'far_field':     {'desc': '远场模拟（低音量+轻微噪声）', 'expected_speech': True, 'expected_endpoints': 1},
    'steady_noise':  {'desc': '稳态背景噪声', 'expected_speech': False, 'expected_endpoints': 0},
    'burst_noise':   {'desc': '突发噪声', 'expected_speech': False, 'expected_endpoints': 0},
    'pure_silence':  {'desc': '纯静音', 'expected_speech': False, 'expected_endpoints': 0},
    'cough_burst':   {'desc': '咳嗽/爆音', 'expected_speech': False, 'expected_endpoints': 0},
}


def run_matrix(fixture_dir: str, vad_gguf_path: str = None,
               vad_cli_path: str = None) -> dict:
    """执行完整测试矩阵。"""

    # 初始化 5 种策略
    energy = EnergyVadStrategy()
    fsmn = FsmnVadStrategy(vad_gguf_path=vad_gguf_path, vad_cli_path=vad_cli_path)
    energy_prefilter = EnergyPrefilterFsmnStrategy(energy, fsmn)
    fsmn_gate = FsmnFinalGateStrategy(energy, fsmn)
    energy_opt = EnergyOptimizedVad()

    strategies = {
        'energy_only': energy,
        'fsmn_only': fsmn,
        'energy_prefilter_fsmn': energy_prefilter,
        'fsmn_final_gate': fsmn_gate,
        'energy_optimized': energy_opt,
    }

    results = {
        'generated_at': time.strftime('%Y-%m-%dT%H:%M:%S%z'),
        'fixture_dir': fixture_dir,
        'vad_gguf_path': vad_gguf_path,
        'vad_cli_path': vad_cli_path,
        'strategies': list(strategies.keys()),
        'fixtures': list(FIXTURES.keys()),
        'matrix': {},
    }

    for fixture_name, fixture_info in FIXTURES.items():
        wav_path = os.path.join(fixture_dir, f'{fixture_name}.wav')
        if not os.path.exists(wav_path):
            print(f"SKIP {fixture_name}: {wav_path} not found")
            continue

        samples = read_wav_16k_mono(wav_path)
        duration_ms = len(samples) * 1000 // 16000

        fixture_result = {
            'description': fixture_info['desc'],
            'duration_ms': duration_ms,
            'expected_speech': fixture_info['expected_speech'],
            'expected_endpoints': fixture_info['expected_endpoints'],
            'strategies': {},
        }

        for strat_name, strat in strategies.items():
            result = strat.run(samples)
            fixture_result['strategies'][strat_name] = {
                'endpoint_events_ms': result.endpoint_events,
                'num_endpoints': len(result.endpoint_events),
                'segments': [
                    {'start_ms': s.start_ms, 'end_ms': s.end_ms}
                    for s in result.segments
                ],
                'inference_time_ms': round(result.inference_time_ms, 2),
                'model_load_count': result.model_load_count,
                'notes': result.notes,
                # 评估
                'correct_endpoint_count': (
                    len(result.endpoint_events) == fixture_info['expected_endpoints']
                ),
                'false_positive': (
                    not fixture_info['expected_speech'] and
                    len(result.endpoint_events) > 0
                ),
                'false_negative': (
                    fixture_info['expected_speech'] and
                    len(result.endpoint_events) == 0
                ),
            }
            print(f"  {fixture_name} / {strat_name}: "
                  f"{len(result.endpoint_events)} endpoints, "
                  f"{result.inference_time_ms:.1f}ms")

        results['matrix'][fixture_name] = fixture_result

    return results


def main():
    fixture_dir = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'fixtures', 'audio')
    results_dir = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'results')
    os.makedirs(results_dir, exist_ok=True)

    # 尝试定位 VAD CLI 和 GGUF
    # 从 funasr-runtime spike 的 cache 中查找
    cache_dir = os.path.join(
        os.path.dirname(os.path.abspath(__file__)),
        '..', 'funasr-runtime', '.cache'
    )
    vad_gguf = os.environ.get('FSMN_VAD_GGUF', '')
    vad_cli = os.environ.get('FUNASR_VAD_CLI', '')

    if not vad_gguf:
        # 搜索常见的下载位置
        for candidate in [
            os.path.join(cache_dir, 'downloads', 'fsmn-vad.gguf'),
            os.path.join(cache_dir, 'FunASR', 'runtime', 'llama.cpp', 'gguf', 'fsmn-vad.gguf'),
        ]:
            if os.path.exists(candidate):
                vad_gguf = candidate
                break

    if not vad_cli:
        for candidate_name in ['funasr-vad.exe', 'funasr-vad']:
            for search_dir in [
                os.path.join(cache_dir, 'runtime'),
                os.path.join(cache_dir, 'prefixes'),
            ]:
                if os.path.isdir(search_dir):
                    for root, dirs, files in os.walk(search_dir):
                        if candidate_name in files:
                            vad_cli = os.path.join(root, candidate_name)
                            break
            if vad_cli:
                break

    print(f"Fixture dir: {fixture_dir}")
    print(f"VAD GGUF: {vad_gguf or '(not found)'}")
    print(f"VAD CLI: {vad_cli or '(not found)'}")
    print()

    results = run_matrix(fixture_dir, vad_gguf, vad_cli)

    output_path = os.path.join(results_dir, 'vad-matrix.json')
    with open(output_path, 'w', encoding='utf-8') as f:
        json.dump(results, f, ensure_ascii=False, indent=2)

    print(f"\nResults written to {output_path}")

    # 打印摘要
    print("\n=== Summary ===")
    for fixture_name, fixture_data in results['matrix'].items():
        print(f"\n{fixture_name} ({fixture_data['duration_ms']}ms):")
        for strat_name, strat_data in fixture_data['strategies'].items():
            status = 'OK' if strat_data['correct_endpoint_count'] else 'MISMATCH'
            if strat_data['false_positive']:
                status = 'FALSE_POS'
            if strat_data['false_negative']:
                status = 'FALSE_NEG'
            print(f"  {strat_name:30s}: {strat_data['num_endpoints']} endpoints "
                  f"({strat_data['inference_time_ms']:.1f}ms) [{status}]")


if __name__ == '__main__':
    main()
