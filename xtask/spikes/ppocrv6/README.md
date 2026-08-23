# PP-OCRv6 Spike — 候选资格门与协议冻结

> **0.22.0** 子版本产物。目标是判断 PP-OCRv6 是否有资格成为 Blink 的第二个本地模型引擎，并冻结后续实现所需的最小服务/请求协议。
>
> **这是 spike，不是生产接入。** 不接入 `main.rs`、Tauri command、`OcrBackendRouter` 或生产启动 wiring。

## 前置条件

- Windows 10/11 x64
- 网络连接（首次安装和模型下载）
- ~3GB 可用磁盘空间（venv + 模型 + 临时下载缓存）

## 目录结构

```text
xtask/spikes/ppocrv6/
├── README.md                 — 本文件
├── requirements.txt          — 精确锁定的 Python 依赖
├── lock.json                 — 包版本/index/wheel URL/SHA-256 记录
├── server_thin.py            — thin Blink HTTP wrapper（拓扑 A）
├── server_paddlex.py         — PaddleX basic serving 适配（拓扑 B）
├── worker_once.py            — 单次 worker 实验（拓扑 C）
├── install.ps1                — 环境安装脚本
├── run_benchmark.ps1         — benchmark 运行脚本
├── evaluate.ps1              — 评价脚本（CER / rect 有效性）
├── cache_tests.ps1           — 缓存验证实验脚本
├── cleanup.ps1               — 清理脚本（venv / 模型 / 缓存）
├── protocol.md               — 最小服务协议草案
├── decision.md               — 资格门逐项判定与最终结论
└── results/                  — 原始 benchmark 结果（git-ignored）
```

## 复现步骤

### 1. 安装环境

```powershell
# 在仓库根目录执行
.\xtask\spikes\ppocrv6\install.ps1
```

脚本使用 Blink/uv 托管 Python，不依赖系统 Python。会：
- 下载 uv（如未安装）
- 创建隔离 venv（`.venv/`）
- 安装锁定的 PaddlePaddle + PaddleOCR + 服务依赖
- 下载并缓存 PP-OCRv6 模型

### 2. 运行 benchmark

```powershell
.\xtask\spikes\ppocrv6\run_benchmark.ps1
```

对三种拓扑 × 三档模型（tiny/small/medium）各运行 ≥10 次，测量：
- service/model 冷启动
- 首次识别 / 热识别延迟
- CPU 占用
- 峰值/稳定工作集
- venv + 模型磁盘占用
- 停止后进程回收

### 3. 评价准确率

```powershell
.\xtask\spikes\ppocrv6\evaluate.ps1
```

使用 `testdata/ocr/ppocrv6/` 中的 golden corpus，输出：
- 各子集 CER（中文/英文/日文/竖排/小字号/浅色深色/DPI）
- 相对 WinRT 的变化
- 有效 word rect 比例
- 越界/空 rect/DPI 偏移

### 4. 缓存验证

```powershell
.\xtask\spikes\ppocrv6\cache_tests.ps1
```

验证：缓存重定向、首次下载、断网离线、损坏缓存、revision 不符、上游失败。

### 5. 清理

```powershell
.\xtask\spikes\ppocrv6\cleanup.ps1
```

删除 venv、模型缓存、uv cache 和临时输出。**不会删除 spike 脚本本身和 golden corpus。**

## 网络需求

- PyPI（`pypi.org` / `pypi.python.org`）—— Python 包
- PaddlePaddle wheel index（`www.paddlepaddle.org.cn` 或镜像）
- PaddleOCR 模型仓库（`paddleocr.bj.bcebos.com` 或 `paddleocr-cn.cdn.bcebos.com`）

## 不提交的内容

以下目录和文件被 `.gitignore` 排除：

- `.venv/` — Python 虚拟环境
- `uv-cache/` — uv 下载缓存
- `model-cache/` — 模型缓存
- `results/` — benchmark 原始输出
- 任何 `.png.bak` 临时文件

## 安全约束

- 未运行 benchmark 时不产生后台 Python 进程
- 不接入生产 wiring（`main.rs` / Tauri command / `OcrBackendRouter`）
- 不改变现有 WinRT OCR 默认行为
- 不实现通用 `LocalEngineService`、`ManagedProcess` 或正式 `PythonRuntime`
