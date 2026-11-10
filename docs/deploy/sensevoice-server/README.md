# sensevoice-server 自部署（云端 STT 流式端点）

Blink 云端语音输入的自部署后端：[FunAudioLLM/SenseVoice](https://github.com/FunAudioLLM/SenseVoice)
`runtime/llama.cpp/sensevoice-server`（C++ 单二进制，SenseVoiceSmall q8 GGUF + FSMN-VAD）。
提供 OpenAI 兼容 HTTP（`/v1/audio/transcriptions`）与 Realtime WebSocket
（`/v1/realtime?intent=transcription`）两种接口——后者是 Blink 云端流式听写使用的协议。

## 部署

```bash
# 1. .env 写入网关密钥
echo "SV_API_KEY=<随机长串>" > .env

# 2. 修改 docker-compose.yml 里两处模型挂载路径为宿主机实际路径

# 3. 构建并启动（model-init 先把模型导出到挂载卷）
docker compose up -d --build
```

| 端口 | 用途 |
|---|---|
| 38040 | 内网直连（**无鉴权**，仅内网用，不要暴露公网） |
| 38041 | nginx 鉴权网关（Bearer `SV_API_KEY` + 限流），对外只暴露这个 |

## Blink 对接

设置页 → 语音输入 → 云端模式：

- 预设选「自定义」，Base URL 填 `http://<内网IP>:38040/v1`（走网关则 `http://<IP>:38041/v1` 并保存 API Key）
- 模型 ID：`sensevoice-small`
- 「流式能力测试」按钮应显示 Realtime WS 支持

## 性能参数（0.29.1 调优）

compose 的 `environment` 覆盖，改完 `docker compose up -d` 重建容器即可，无需重建镜像：

| 变量 | 默认 | 说明 |
|---|---|---|
| `SV_VAD_MAXSEG` | `8000` | 强制切段上限（ms）。**最关键的旋钮**：服务端 FSMN-VAD 的端点静音阈值写死且以 60s 为步长跳变——会话前 60 秒恒需 ~1850ms 静音才切段，自然停顿达不到；而 WS partial 对整个开放段同步重识别并阻塞读循环，段越长积压越重。8s 上界保证单次 partial ≤~0.6s、定稿及时 |
| `SV_PARTIAL_MS` | `600` | 预览节流（服务端默认 400） |
| `SV_THREADS` | `4` | 推理线程，按宿主机物理核数调 |
| `SV_MAX_CONN` | `4` | 并发连接上限 |

调参依据（源码审查 + 真实语料实测）见 [docs/phases/0.29-cloud-stt-realtime-streaming.md §3.6 / §六](../../phases/0.29-cloud-stt-realtime-streaming.md)。
