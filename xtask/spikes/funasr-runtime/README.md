# FunASR GGUF Server Quick Spike

0.22.7 的最小可行性实验，只回答两个问题：

1. 官方最新 `funasr_gguf_server.py` 能否被 Blink 当前 OpenAI-compatible STT 请求直接调用；
2. 每次请求重新启动 `llama-funasr-sensevoice` 的拓扑能否提供可用的伪流式预览。
3. SenseVoice GGUF 能否在一个原生进程中常驻并连续识别多个音频快照。

本实验不做生产接入、不比较 CER/WER，也不覆盖并发性能、完整安装治理或 GPU profile。

## 当前实验输入

- 官方 wrapper：`modelscope/FunASR` 的 `main/runtime/llama.cpp/server/funasr_gguf_server.py`
- Windows runtime：最新 release `runtime-llamacpp-v0.2.6` 的 portable x64 CPU 包
- ASR：SenseVoiceSmall Q8 GGUF
- VAD：FSMN-VAD GGUF
- Python：Blink 已托管的 CPython 3.12.8，以 `-I -S` 启动，不创建 venv、不执行 pip
- 音频：由 Windows SAPI 本地生成的固定 WAV，不含用户录音

## 运行

先准备 `.cache/downloads/` 和 `.cache/runtime/` 中的官方文件，然后执行：

```powershell
.\xtask\spikes\funasr-runtime\quick_spike.ps1
```

结果写入 `results/quick-spike.json`。脚本依次发送 0.5、1、2、3、4 秒以及完整音频，模拟 Blink 从首次 500ms preview 开始，对“当前未确认音频快照”的重复识别。

## 判定边界

Blink 当前 `PseudoStreamingSttEngine` 同一时刻只允许一个 preview 请求在途。wrapper 每次请求都会冷启动一次 C++ 推理进程，因此实际预览频率受单次请求耗时约束，而不是代码中的 500ms 调度间隔。

## 常驻 worker 复验

基于 FunASR `55b662ccf9ea77237ba9253b3bddd953d4184f84`，应用
`sensevoice-persistent-worker.patch` 后编译 CPU 版。该补丁只增加按行读取 WAV
路径的 `--stdin-server` 循环；模型加载、fbank、计算图和 CTC 解码保持原样。

实测同一个 PID 依次处理 0.5s、1s、2s、5.708s、0.5s、5.708s 六个请求，
模型加载日志只出现一次，全部返回一致文本。常驻进程 working set 约 251 MiB。
详细延迟和结论见 `decision.md`。
