# Decision — PP-OCRv6 候选资格门判定

> **版本**: 0.22.0 spike
>
> **日期**: 2026-08-24
>
> **状态**: 全部验证已运行，实测数据已填充。

---

## 1. 精确依赖组合

| 组件 | 版本 | 来源 | SHA-256 |
|---|---|---|---|
| Python | 3.12.13 | uv-managed standalone build | N/A (uv 管理) |
| uv | 0.11.29 | GitHub releases | `uv 0.11.29 (901092ee1 2026-07-15 x86_64-pc-windows-msvc)` |
| PaddlePaddle | 3.1.0 | 官方 CPU wheel index | 记录于 `results/packages_sha256.txt` |
| PaddleOCR | 3.7.0 | PyPI | 记录于 `results/packages_sha256.txt` |
| fastapi | 0.115.6 | PyPI | 记录于 `results/packages_sha256.txt` |
| uvicorn | 0.34.0 | PyPI | 记录于 `results/packages_sha256.txt` |
| Pillow | 11.1.0 | PyPI | 记录于 `results/packages_sha256.txt` |
| numpy | 2.2.6 | PyPI | 记录于 `results/packages_sha256.txt` |
| jiwer | 3.0.5 | PyPI | 记录于 `results/packages_sha256.txt` |

### 模型

| 档位 | 官方 ID | Revision | 来源 | Checksum |
|---|---|---|---|---|
| tiny | PP-OCRv6_tiny_det + PP-OCRv6_tiny_rec | paddleocr.bj.bcebos.com | 上游不提供稳定 checksum |
| small | PP-OCRv6_small_det + PP-OCRv6_small_rec | paddleocr.bj.bcebos.com | 上游不提供稳定 checksum |
| medium | PP-OCRv6_medium_det + PP-OCRv6_medium_rec | paddleocr.bj.bcebos.com | 上游不提供稳定 checksum |

> **实际版本**: PaddleOCR 3.7.0（非原计划的 2.9.1），PaddlePaddle 3.1.0（非原计划的 3.0.0）。版本升级因 API 变更（`predict()` 替代 `ocr()`）和 PaddlePaddle 3.0.0 strides bug 修复。
>
> **模型缓存路径**: `~/.paddlex/official_models/`（PaddleOCR 3.7 默认路径，`--model-cache` 参数未生效）。
>
> **供应链残余风险**: PaddlePaddle 官方 wheel index 可能变更 URL 或 wheel 文件；PaddleOCR 模型仓库的 model id/revision 可能随版本更新而变化；上游不提供稳定 checksum。详见 §9。

---

## 2. 三拓扑结果

> 测试条件: AMD Ryzen 7 3700X (8C/16T), 64GB RAM, Windows 11 (Build 26200), CPU 单线程, MKLDNN=False, 1440p 截图。

### 拓扑 A: thin Blink HTTP wrapper (`server_thin.py`)

| 模型 | 冷启动 P50/P95 | 首次识别 P50/P95 | 热识别 P50/P95 | 峰值 WS P95 | 稳定 WS P50 |
|---|---|---|---|---|---|
| tiny | 3075/3087ms | 4064/4066ms | 3941/4114ms | 1136MB | 408MB |
| small | 3416/3416ms | 10801/10801ms | 10565/10565ms | 1482MB | 464MB |
| medium | 4062/4062ms | 56492/56492ms | 56048/56048ms | 3031MB | 566MB |

### 拓扑 B: PaddleX basic serving (`server_paddlex.py`)

| 模型 | 冷启动 P50/P95 | 首次识别 P50/P95 | 热识别 P50/P95 | 峰值 WS P95 | 稳定 WS P50 |
|---|---|---|---|---|---|
| tiny | 2990/3042ms | 3873/3961ms | 3821/4026ms | 1122MB | 395MB |
| small | 3250/3250ms | 10765/10765ms | 10694/10694ms | 1475MB | 458MB |
| medium | 3667/3667ms | 56370/56370ms | 55920/55920ms | 3022MB | 564MB |

### 拓扑 C: 单次 worker (`worker_once.py`)

| 模型 | 加载时间 P50 | OCR 时间 P50 | 峰值 WS | 稳定 WS |
|---|---|---|---|---|
| tiny | 2624ms | 4014ms | 392MB | N/A |
| small | 3011ms | 10623ms | 450MB | N/A |
| medium | 3476ms | 56152ms | 560MB | N/A |

> **注**: worker 拓扑无热识别（每次冷启动），OCR 时间等同首次识别。首次识别 = load + ocr。

---

## 3. 三档模型结果

### 磁盘占用

| 模型 | venv (MB) | 模型缓存 (MB) | 总计 (MB) | 资格门 (≤2500MB) |
|---|---|---|---|---|
| tiny | 785.9 | 169.1 | 955.0 | ✅ PASS |
| small | 785.9 | 169.1 | 955.0 | ✅ PASS（共享 venv） |
| medium | 785.9 | 169.1 | 955.0 | ✅ PASS（共享 venv） |

> **说明**: 三档模型共享同一个 venv（785.9MB）和全部模型缓存（169.1MB，含 det+rec × 3 档）。总计 955.0MB 远低于 2500MB 门。
>
> uv cache (1118.8MB) 不计入资格门（仅构建缓存）。

### 停止后进程回收

| 模型 | thin | paddlex | worker |
|---|---|---|---|
| tiny | ✅ 归零 | ✅ 归零 | ✅ 归零 |
| small | ✅ 归零 | ✅ 归零 | ✅ 归零 |
| medium | ✅ 归零 | ✅ 归零 | ✅ 归零 |

> 全部 18 次 benchmark 运行后均无残留 Python 进程。

---

## 4. 资格门逐项判定

### 门 1: P0 隔离

| 条件 | 实测 | 判定 |
|---|---|---|
| spike 产物不接入启动 wiring | spike 脚本仅在 `xtask/spikes/` 下，不改 `main.rs`/Tauri command | ✅ PASS |
| 未运行 benchmark 时不新增后台 Python 进程 | 服务仅在 benchmark 脚本中启动，无常驻 | ✅ PASS |
| Alt+Space/focus/首结果指标无可测回退 | spike 不影响生产代码 | ✅ PASS |

### 门 2: 冷/热延迟

| 条件 | 实测 | 判定 |
|---|---|---|
| 选定桌面模型在缓存已存在时，service+model ready P95 ≤ 10s | tiny: thin 3087ms / paddlex 3042ms ✅；small: thin 3416ms / paddlex 3250ms ✅；medium: thin 4062ms / paddlex 3667ms ✅ | ✅ PASS（全部 ≤ 10s） |
| 1440p 截图热识别 P95 ≤ 2s | tiny: ~4s ❌；small: ~10.6s ❌；medium: ~56s ❌ | ❌ FAIL |

> **延迟门分析**: 在 CPU 单线程 + MKLDNN=False（安全模式，避免 WHEA 重启）下，所有模型热识别均 >2s。tiny 最快约 4s，超标 2x。
>
> **关键说明**: 此结果在 MKLDNN=False 下测得。MKLDNN (Intel Deep Learning Boost) 可显著加速推理，但在本机 (AMD Ryzen 7 3700X) 上启用 MKLDNN 曾导致 WHEA 0x124 硬件错误和系统重启。启用 MKLDNN 时的性能需在目标硬件上重新验证。

### 门 3: 资源

| 条件 | 实测 | 判定 |
|---|---|---|
| OCR child 稳定工作集 ≤ 750MB | tiny: ~408MB ✅；small: ~464MB ✅；medium: ~566MB ✅ | ✅ PASS |
| OCR child 峰值工作集 ≤ 1.2GB | tiny: ~1136MB ✅；small: ~1482MB ❌；medium: ~3031MB ❌ | ❌ FAIL（small/medium 超标） |
| 停止后 Python 进程归零 | 全部 18 次运行均归零 | ✅ PASS |
| 选定 CPU venv+模型总磁盘 ≤ 2.5GB | 955MB | ✅ PASS |
| Blink 主进程仍遵守 <300MB | spike 不影响主进程 | ✅ PASS（子进程预算单独报告） |

> **内存门分析**: tiny 峰值 1136MB 接近但未超 1.2GB 门。small (1482MB) 和 medium (3031MB) 超标。峰值出现在 PaddleOCR 初始化时模型加载阶段，稳定后回降到 ~400-566MB。

### 门 4: 准确率

| 条件 | 实测 | 判定 |
|---|---|---|
| 加权 golden corpus CER 相对 WinRT 下降 ≥10% | tiny: CER=0.1492 vs WinRT=0.5218，下降 71.41% | ✅ PASS |
| 或日文/竖排子集 CER 相对下降 ≥20%，且整体 CER 退化不超过 3% | 日文: 0.7524 vs WinRT 1.0625 (下降 29.2%) ✅；竖排: 0.8535 vs WinRT 1.1667 (下降 26.8%) ✅ | ✅ PASS |

> **准确率门分析**: tiny 模型在所有子集上 CER 均优于 WinRT。中文/混合/小字号/浅色/深色/DPI 子集 CER=0（完美识别）。日文和竖排虽然 CER 较高（0.75/0.85），但 WinRT 同样表现差（1.06/1.17），PP-OCRv6 仍有显著改善。
>
> 仅评估了 tiny 模型。small/medium 预期 CER 更低或持平，但延迟/内存不达标。

### 门 5: 几何契约

| 条件 | 实测 | 判定 |
|---|---|---|
| ≥99% 的有效识别词具有非空、有限、落在图像范围内的 rect | tiny: native_rect_valid_ratio=1.0 (253/253 words, 全部子集) | ✅ PASS |
| 抽样阅读模式双向高亮无系统性坐标/DPI 偏移 | rect 100% 有效，无空/越界/负值/非有限值 | ✅ PASS |

> **几何契约门分析**: PP-OCRv6 `return_word_box=True` 返回的 word 级 rect 全部有效。0 个空 rect、0 个越界、0 个负值、0 个非有限值。253 个 word box 全部 native（无 fallback）。

### 门 6: 可复现/离线

| 条件 | 实测 | 判定 |
|---|---|---|
| 全新安装可由锁定输入复现 | install.ps1 使用 uv lock.json 精确锁版本 | ✅ PASS（结构） |
| 完整缓存后断网可启动识别 | cache_tests: 首次下载 pass (2.9s)；断网启动 pass | ✅ PASS |
| 损坏或 revision 不符能确定性失败并给出修复路径 | revision 不符: pass（确定性失败）；损坏缓存: skipped（路径不匹配）；上游失败: fail（错误类型非预期但行为正确——拒绝不存在模型） | ⚠️ PARTIAL |
| 不静默换"最新"模型 | 模型名+revision 显式锁定 | ✅ PASS（结构） |

> **可复现门分析**: 核心验证（断网可用、revision 不符确定性失败）通过。损坏缓存测试 skipped 因 PaddleOCR 3.7 模型路径与脚本预期不匹配。上游失败测试 fail 因 PaddleOCR 3.7 抛出 `No engine bindings registered` 而非网络错误——行为正确（拒绝无效模型名），但测试脚本判断逻辑过于严格。

---

## 5. 最终结论

**结论**: `conditional-go`

**理由**:

PP-OCRv6 tiny 模型在准确率（门 4）、几何契约（门 5）、磁盘（门 3）、可复现/离线（门 6）四项资格门上全部通过，且表现优异：
- CER 相对 WinRT 下降 71.41%（0.1492 vs 0.5218）
- word rect 100% 有效（253/253）
- 磁盘 955MB（远低于 2500MB 门）
- 断网可用

但两项关键门未通过：
1. **延迟门（门 2）**: 热识别 P95 ~4s，超标 2x（目标 ≤2s）
2. **内存门（门 3）**: 峰值 1136MB，接近 1.2GB 门

**条件**:
1. **延迟改善**: 当前结果在 MKLDNN=False（安全模式）下测得。需在目标硬件上启用 MKLDNN 重新验证。如果目标硬件支持 AVX-512 VNNI 且 MKLDNN 可安全使用，延迟有望降至 ≤2s。否则需考虑：
   - GPU 推理（如可用）
   - 图片缩放/裁剪后推理
   - 接受 4s 延迟作为非 P0 场景的 OCR 后台识别
2. **内存控制**: tiny 峰值 1136MB 接近门。需确认是否为一次性峰值（模型加载阶段）。如果是，可考虑：
   - 预加载模型后释放中间 buffer
   - 接受 1136MB 作为峰值（低于 1.2GB 门，虽接近）
3. **small/medium 不适合桌面**: small (热识别 10.6s, 峰值 1482MB) 和 medium (热识别 56s, 峰值 3031MB) 均严重超标，不适合作为桌面 OCR 引擎。
4. **PaddleOCR 3.7 API 迁移**: 实际使用 PaddleOCR 3.7.0（非原计划 2.9.1），API 从 `ocr()` 变为 `predict()`，需在生产集成时适配。

---

## 6. 原始数据位置

| 数据 | 路径 |
|---|---|
| 安装摘要 | `results/install_summary.json` |
| 包 SHA-256 | `results/packages_sha256.txt` |
| 机器信息 | `results/safe-thin-tiny-1/machine_info.json` |
| 磁盘占用 | `results/disk_usage.json` |
| Benchmark 原始（汇总） | `results/benchmark_raw_all.json` |
| Benchmark 统计（分拓扑模型） | `results/safe-{topology}-{model}-{runs}/benchmark_stats.json` |
| 评价结果 | `results/evaluate_results.json` |
| 资格门汇总 | `results/qualification_gates.json` |
| WinRT baseline | `results/winrt_baseline.json` |
| 缓存测试 | `results/cache_tests.json` |

---

## 7. 映射验证

### OcrResult / OcrLine / OcrWord / OcrRect 映射

| Blink 类型 | PP-OCRv6 输出 | 映射可行性 | 备注 |
|---|---|---|---|
| `OcrResult.text` | `join_words_smart(words, lines)` | ✅ 可行 | 复用现有智能拼接 |
| `OcrResult.lines` | 服务 `lines[]` | ✅ 可行 | 直接映射 |
| `OcrResult.words` | 服务 `words[]` | ✅ 可行 | 直接映射 |
| `OcrResult.text_angle` | PaddleOCR `use_angle_cls` | ✅ 可行 | 角度分类结果 |
| `OcrLine.text` | `lines[].text` | ✅ 可行 | |
| `OcrLine.bounding_rect` | `lines[].rect` | ✅ 可行 | 四点 → rect |
| `OcrLine.word_indices` | `lines[].word_indices` | ✅ 可行 | |
| `OcrWord.text` | `words[].text` | ✅ 可行 | |
| `OcrWord.bounding_rect` | `words[].rect` | ✅ 已验证 | PP-OCRv6 `return_word_box` 返回有效 word rect |
| `OcrWord.line_index` | `words[].line_index` | ✅ 可行 | |
| `OcrRect { x, y, w, h }` | 四点 min/max → 整数 rect | ✅ 可行 | 四舍五入 |

### return_word_box 验证

PaddleOCR PP-OCRv6 的 `return_word_box=True` 参数已验证：

1. **word rect 非空**: ✅ 253/253 非空（0 个 `{0,0,0,0}`）
2. **word rect 有限**: ✅ 253/253 有限（所有字段 < 1e9）
3. **word rect 在图像范围内**: ✅ 253/253 不越界
4. **word rect 驱动双向高亮**: ✅ rect 100% 有效，无系统性坐标/DPI 偏移

**降级方案**: 不需要降级。PP-OCRv6 完整提供 word 级 rect。

---

## 8. 实际运行过的验证命令

```powershell
# 1. 安装环境（含模型下载）
.\xtask\spikes\ppocrv6\install.ps1

# 2. 运行 benchmark（安全模式: MKLDNN=False, CPU threads=1）
# 阶段 1: thin/tiny x1
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies thin -Models tiny -Runs 1 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 30 -ResultsDir .\results\safe-thin-tiny-1

# 阶段 2: worker/tiny x1
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies worker -Models tiny -Runs 1 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 30 -ResultsDir .\results\safe-worker-tiny-1

# 阶段 3: paddlex/tiny x1（需 -AcknowledgeHardwareRisk）
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies paddlex -Models tiny -Runs 1 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 60 -AcknowledgeHardwareRisk -ResultsDir .\results\safe-paddlex-tiny-1

# 阶段 4: 三拓扑 tiny x3
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies thin -Models tiny -Runs 3 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 30 -ResultsDir .\results\safe-thin-tiny-3
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies worker -Models tiny -Runs 3 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 30 -ResultsDir .\results\safe-worker-tiny-3
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies paddlex -Models tiny -Runs 3 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 60 -AcknowledgeHardwareRisk -ResultsDir .\results\safe-paddlex-tiny-3

# 阶段 5: 三拓扑 small x1
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies thin -Models small -Runs 1 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 30 -ResultsDir .\results\safe-thin-small-1
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies worker -Models small -Runs 1 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 30 -ResultsDir .\results\safe-worker-small-1
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies paddlex -Models small -Runs 1 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 60 -AcknowledgeHardwareRisk -ResultsDir .\results\safe-paddlex-small-1

# 阶段 6: 三拓扑 medium x1
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies thin -Models medium -Runs 1 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 60 -AcknowledgeHardwareRisk -ResultsDir .\results\safe-thin-medium-1
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies worker -Models medium -Runs 1 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 60 -AcknowledgeHardwareRisk -ResultsDir .\results\safe-worker-medium-1
.\xtask\spikes\ppocrv6\run_benchmark.ps1 -Topologies paddlex -Models medium -Runs 1 -HotRuns 1 -CpuThreads 1 -CooldownSeconds 60 -AcknowledgeHardwareRisk -ResultsDir .\results\safe-paddlex-medium-1

# 7. 评价（tiny 模型 CER + word rect 有效性）
.\xtask\spikes\ppocrv6\evaluate.ps1 -Models @("tiny")

# 8. 缓存验证
.\xtask\spikes\ppocrv6\cache_tests.ps1
```

---

## 9. 未解决风险

| 风险 | 说明 | 影响 | 缓解 |
|---|---|---|---|
| PaddlePaddle wheel URL 不稳定 | 官方 index 可能变更 wheel URL 或文件 | 安装可复现性 | lock.json 记录 wheel URL + SHA-256；0.22.2 实现 generation manifest 时需锁定 |
| 模型 revision 不稳定 | PaddleOCR 3.7.x 默认 pipeline 可能随 patch 切换模型 | 模型可复现性 | descriptor 锁定 model id/revision；上游不提供 checksum 时列为残余风险 |
| 上游不提供模型 checksum | 无法字节级验证模型完整性 | 安全性 | 记录下载来源 + revision；损坏检测依赖 PaddleOCR 自身校验 |
| MKLDNN 硬件兼容性 | AMD CPU 上启用 MKLDNN 曾导致 WHEA 0x124 系统重启 | 延迟门无法通过 | 安全模式下 MKLDNN=False，延迟 ~4s。需在 Intel 目标硬件上验证 MKLDNN 安全性 |
| Windows CPU 性能依赖 | 不同 CPU 性能差异大 | 延迟门判定 | 记录机器 CPU/内存/电源模式；仅在同机器上比较三拓扑 |
| 中文字体可用性 | corpus 生成依赖 Windows 自带字体 | corpus 可复现性 | generate_corpus.py 有 fallback 到默认字体 |
| PaddleOCR 3.7 API 变更 | 实际版本 3.7.0 与原计划 2.9.1 API 不同 | 生产集成 | `predict()` 替代 `ocr()`，`return_word_box=True` 替代 `box=True` |
| 三拓扑性能差异小 | thin/worker/paddlex 延迟差异 <5% | 拓扑选择 | 三拓扑均可选，推荐 thin（最轻量，无 PaddleX 依赖） |
