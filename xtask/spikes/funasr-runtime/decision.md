# Decision — FunASR GGUF Server 快速可行性验证

> 日期：2026-08-30  
> 结论：`conditional-go`  
> 范围：只判断官方 GGUF HTTP wrapper 与 Blink 现有伪流式调用是否可行，不代表生产接入验收完成。

> 常驻性复验：`pass`。llama.cpp/GGML 模型可以在一个原生进程内只加载一次并连续处理请求。

## 结论

官方最新 `funasr_gguf_server.py` **可以直接服务 Blink 当前的 OpenAI-compatible transcription 请求，也可以用于现有 `PseudoStreamingSttEngine` 的伪流式预览**。

但它只能提供约 1～2 秒级的粗粒度预览，不能提供 500ms 一次的真正增量体验。原因不是 Blink 的调度，而是官方 wrapper 每个请求都会重新启动一次 `llama-funasr-sensevoice`，模型加载和推理耗时高于 Blink 的 500ms preview interval。

因此建议：

- 可以继续做 `funasr-gguf` 的最小接入，首版标记为 `final-only / coarse preview`；
- 可以复用当前 `PseudoStreamingSttEngine`，不需要新增流式协议；
- 不应宣称 realtime、partial streaming 或 500ms 文本刷新；
- 若产品要求稳定低于 1 秒的连续预览，需要模型常驻的 server，而不是当前每请求 spawn 的 wrapper。

## 实验组合

- wrapper：`modelscope/FunASR` 当前 `main/runtime/llama.cpp/server/funasr_gguf_server.py`
- wrapper SHA-256：`3704d6607c033c2c3bd208674cd65af8b2cc40b27365a213fd473b51a9459f70`
- runtime：`runtime-llamacpp-v0.2.6` Windows x64 portable CPU
- ASR：SenseVoiceSmall Q8 GGUF
- VAD：FSMN-VAD GGUF
- Python：Blink 托管 CPython 3.12.8，使用 `python -I -S`
- 固定音频：5.708 秒，由 Windows SAPI 本地生成，不含用户录音

## 协议 smoke

以下合同已实测通过：

- `GET /health` 返回 HTTP 200：`{"status":"ok"}`；
- `POST /v1/audio/transcriptions` 接受 Blink 当前的 multipart `file + model`；
- 携带现有 `X-Engine-Token` header 不影响调用；
- 返回 `{"text":"..."}`，可由当前 client 直接解析；
- 文本非空、UTF-8 合法；
- 每次请求结束后临时 WAV 归零。

wrapper ready 为约 199ms，但这只表示 HTTP 端口可用，**不表示模型已经加载**。

## 伪流式快照结果

| 输入快照 | HTTP 耗时 | 返回文本 |
|---|---:|---|
| 0.5s | 858.6ms | `Hello.` |
| 1.0s | 1024.2ms | `Hello.` |
| 2.0s | 1299.1ms | `Hello, this is.` |
| 3.0s | 1544.5ms | `Hello, Jack, this is a fixed speech.` |
| 4.0s | 1875.6ms | `Hello, Jack, this is a fixed speech recognition.` |
| 5.708s | 2218.7ms | `Hello, J, this is a fixed speech recognition test for blink.` |

结果证明随着音频增长可以返回逐步扩展的文本，因此“重复识别当前音频快照”的伪流式语义成立。

Blink 当前同一时刻只允许一个 preview 请求在途。首次 preview 在录音约 500ms 时触发，结合本机 858.6ms 的首个请求耗时，最早可见文本约在录音开始后 1.36 秒出现。后续刷新也会受每次推理耗时限制，实际约为 1～2 秒级，而不是每 500ms 一次。

完整原始结果见本机生成的 `results/quick-spike.json`。

## 常驻模型复验结果

为了区分“llama.cpp 做不到常驻”和“官方 CLI 没有实现常驻”，本实验直接在
FunASR `55b662ccf9ea77237ba9253b3bddd953d4184f84` 的 SenseVoice C++ 入口增加
最小 `--stdin-server` 循环。模型和 backend 初始化仍在循环外，单次推理函数未改。

同一个 PID 连续完成六次请求，`[sensevoice] loading model metadata` 与
`model ready: 919 tensors` 都只出现一次；退出前 working set 为 263,401,472 bytes
（约 251 MiB）。重复输入的文本和耗时稳定：

| 输入 | 常驻 worker 内部耗时 | 文本 |
|---|---:|---|
| 0.5s（首次） | 46ms | `Hello.` |
| 1.0s | 66ms | `Hello.` |
| 2.0s | 110ms | `Hello, this is.` |
| 5.708s（首次） | 272ms | `Hello, J, this is a fixed speech recognition test for blink.` |
| 0.5s（重复） | 46ms | `Hello.` |
| 5.708s（重复） | 267ms | `Hello, J, this is a fixed speech recognition test for blink.` |

作为同机对照，官方一次性 CPU CLI 对同一个 0.5s WAV 连跑五次的端到端耗时为
848.4、827.5、817.2、792.7、781.6ms；中位数 817.2ms。常驻 worker 为 46ms，
约快 17.8 倍。该差值主要来自每请求进程启动、GGUF 读取、权重分配和 backend 初始化。

因此常驻在技术上已验证可行。当前官方 Python wrapper 的慢路径是实现选择，不是
llama.cpp/GGML 限制。下一步产品化应采用“Blink 管理一个常驻原生 worker，HTTP
wrapper 或 Rust adapter 只负责转发请求”；模型切换时重启 worker 即可。

## 接入前仍需补的最小缝隙

这些不阻断本次可行性结论，但生产接入时必须处理：

1. `/health` 只返回 `status`，没有 `engine_id`、`instance_id`、模型身份或 actual backend，不能直接满足 Blink 当前完整 health 身份合同。
2. wrapper 不校验 `X-Engine-Token`；当前 header 只是被忽略。
3. multipart 的 `model` 字段被接受但不参与选择，实际模型在 wrapper 启动参数中固定。
4. wrapper 使用阻塞的 `subprocess.run`。调用方取消 HTTP future 时，服务端推理子进程不会因此立即取消；停止服务时仍需依靠 Blink `ManagedProcess`/Job Object 回收进程树。
5. preview 文本会在不同快照间修订，例如 `Jack` 最终变为 `J`；应继续使用现有“latest preview 可替换、final 才确认”的 UI 语义。

## 不建议现在做的事

- 不需要因为本次结果立刻转向 C++ ONNX server；GGUF 主链路已经证明可调用。
- 不需要新增一套伪流式实现；先复用现有 `PseudoStreamingSttEngine`。
- 不需要为了 500ms 调度制造并发请求；并发只会同时启动更多冷进程，不能解决模型不常驻的问题。
- 不应在本阶段扩展 CUDA/Vulkan、并发吞吐或准确率评价。
