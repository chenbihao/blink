# 最小服务协议草案 — PP-OCRv6 Local Engine

> **版本**: 0.2.0-spike
>
> **状态**: 冻结草案（0.22.0 spike 产物，0.22.3 正式实现时以此为基础）
>
> **范围**: 定义 Blink 与 PP-OCRv6 本地 OCR 服务之间的最小 HTTP 协议。

## 1. 端点

### 1.1 `GET /health`

**认证**: 要求 `X-Engine-Token` header。

**响应**（200 OK）:

```json
{
  "protocol_version": "0.2.0",
  "engine_id": "paddleocr-ppocrv6",
  "instance_id": "uuid-4",
  "service_state": "healthy",
  "model_state": "Ready",
  "model_id": "PP-OCRv6",
  "model_revision": "cache_files:N",
  "uptime_seconds": 12.34
}
```

**model_state 值**:

| 值 | 含义 |
|---|---|
| `NotLoaded` | 服务已启动，模型尚未加载 |
| `Loading` | 模型正在加载到内存 |
| `Ready` | 模型已就绪，可接受识别请求 |
| `Failed` | 模型加载失败 |

> **注意**: 当前 spike 实现不支持 `Downloading` 状态。模型下载在 `PaddleOCR()` 构造时同步完成，无法区分下载和加载阶段。0.22.3 正式实现时如需区分，需在 adapter 层增加下载进度探测。

**错误**:

| HTTP | 含义 |
|---|---|
| 401 | token 不匹配 |
| 503 | 服务未就绪（_TOKEN 未设置） |

### 1.2 `POST /recognize`

**认证**: 要求 `X-Engine-Token` header。

**请求**:

```json
{
  "image": "<base64-encoded PNG>",
  "request_id": "optional-uuid",
  "timeout_ms": 30000
}
```

| 字段 | 类型 | 必需 | 说明 |
|---|---|---|---|
| `image` | string | ✅ | base64 编码的 PNG 图片 |
| `request_id` | string | ❌ | 请求追踪 ID，不传则服务端生成 |
| `timeout_ms` | int | ❌ | 请求超时预算（毫秒，相对值）。超时后服务端放弃并返回错误。默认 30000ms |

**图片限制**:
- 格式: PNG
- 最大大小: 20MB（base64 后约 27MB）
- 单次请求一张图片

**响应**（200 OK）:

```json
{
  "request_id": "uuid-4",
  "lines": [
    {
      "text": "识别到的行文本",
      "rect": { "x": 10, "y": 20, "w": 100, "h": 30 },
      "word_indices": [0, 1, 2],
      "confidence": 0.98
    }
  ],
  "words": [
    {
      "text": "word1",
      "rect": { "x": 10, "y": 20, "w": 50, "h": 30 },
      "line_index": 0
    }
  ],
  "elapsed_ms": 123.45,
  "engine": "ppocrv6-thin",
  "model": "small",
  "native_word_boxes": 5,
  "fallback_word_boxes": 0
}
```

**输出契约**:

- `lines`: 行级结果数组
  - `text`: 行文本
  - `rect`: 物理像素矩形 `{ x, y, w, h }`（整数）
  - `word_indices`: 指向 `words` flat 数组的索引段
  - `confidence`: 置信度（0-1）
- `words`: 词级结果 flat 数组
  - `text`: 词文本
  - `rect`: 物理像素矩形
  - `line_index`: 所属行索引
- `native_word_boxes`: 来自 PaddleOCR `return_word_box=True` 的原生 word box 数量
- `fallback_word_boxes`: 因 PaddleOCR 未返回 word box 而从 line rect 拆分得到的 fallback word box 数量

**映射到 Blink `OcrResult`**:

| 服务输出 | Blink domain 类型 |
|---|---|
| `lines[].text` | `OcrLine.text` |
| `lines[].rect` | `OcrLine.bounding_rect` (`OcrRect { x, y, w, h }`) |
| `lines[].word_indices` | `OcrLine.word_indices` |
| `words[].text` | `OcrWord.text` |
| `words[].rect` | `OcrWord.bounding_rect` |
| `words[].line_index` | `OcrWord.line_index` |

**稳定错误码**:

| code | HTTP | 含义 |
|---|---|---|
| `invalid_token` | 401 | token 不匹配 |
| `service_not_ready` | 503 | 服务或模型未就绪 |
| `invalid_base64_image` | 400 | 图片解码失败 |
| `image_too_large` | 413 | 图片超过 20MB |
| `ocr_failed` | 500 | OCR 引擎执行失败 |
| `timeout_exceeded` | 408 | 请求超过 timeout_ms 预算 |
| `model_failed` | 503 | 模型加载失败 |
| `model_not_ready` | 503 | 模型尚未 Ready |

### 1.3 `POST /shutdown`

**认证**: 要求 `X-Engine-Token` header。

**响应**（200 OK）:

```json
{
  "status": "shutting_down"
}
```

服务在返回响应后应在短延迟（<500ms）内退出进程。

## 2. 语义约束

### 2.1 一次 recognize 对应一个 future

每个 `/recognize` 请求对应一个独立的异步 future。服务端不设计图片"重放"队列——请求到达后直接执行 OCR，不复制图片来等待模型启动。

### 2.2 timeout/cancel 语义

- `timeout_ms` 表示请求的**相对超时预算**（从请求到达服务端开始计算），不是绝对 deadline
- 超时后服务端通过 `asyncio.wait_for` 取消等待并返回 `timeout_exceeded`（408）
- OCR 同步 CPU 调用在 worker thread 中执行（`loop.run_in_executor`），不阻塞 FastAPI event loop

**取消边界**（如实描述）：

- **客户端取消**：客户端可以在 timeout 前断开连接（TCP RST/FIN），服务端检测到后会取消对 worker thread 结果的等待
- **"停止等待"与"终止计算"的区别**：
  - `asyncio.wait_for` 超时后取消的是 **event loop 层面对结果的等待**，即服务端不再等待 worker thread 返回结果
  - **底层 PaddleOCR 推理无法安全中断**：worker thread 中的 `engine.predict()` 一旦开始，无法从外部安全终止。超时后该线程仍会继续执行完毕，但其结果会被丢弃
  - 这意味着：超时不会立即释放 CPU，worker thread 会在后台完成推理后自然退出
  - 0.22.3 正式实现时，如需更强取消保证，需评估进程级隔离（子进程可 kill）

### 2.3 model not ready 时不发送业务请求

- 客户端必须先通过 `/health` 确认 `model_state == "Ready"` 后才发送 `/recognize`
- 服务端在 `model_state != "Ready"` 时对 `/recognize` 返回 503

### 2.4 token

- 每次服务启动生成随机 token
- 所有 `/health`、`/recognize`、`/shutdown` 请求都要求携带 `X-Engine-Token` header
- token 不匹配返回 401
- 目的：防止端口上运行的非 Blink 服务被误调用

### 2.5 模型加载确定性

- 模型加载只在服务启动时发生一次（后台线程初始化）
- 并发请求不会重复初始化模型（single-flight via `_ENGINE_LOCK`）
- 模型加载失败后进入 `Failed` 状态，不会自动重试

## 3. 不包含的设计

### 3.1 不设计图片重放队列

不在服务端设计"先存图片、等模型 ready 后批量处理"的队列。客户端在 model not ready 时不发送图片，而是轮询 health 等待 ready。

### 3.2 不复制请求图片来等待模型启动

模型启动期间不缓存请求图片。客户端持有唯一图片副本，在 model ready 后发送。

### 3.3 不伪造下载百分比

模型下载阶段无法取得字节级进度时只展示阶段（`Loading`），不伪造百分比。当前 spike 实现不区分 `Downloading` 和 `Loading` 阶段。

## 4. 与 Blink 现有 OcrResult 契约的映射验证

### 4.1 OcrResult

```rust
pub struct OcrResult {
    pub text: String,           // ← join_words_smart(words, lines) 智能拼接
    pub lines: Vec<OcrLine>,    // ← 服务 lines[]
    pub words: Vec<OcrWord>,    // ← 服务 words[]
    pub text_angle: Option<f64>, // ← PaddleOCR use_angle_cls 结果
}
```

### 4.2 OcrLine

```rust
pub struct OcrLine {
    pub text: String,                      // ← lines[].text
    pub bounding_rect: OcrRect,            // ← lines[].rect
    pub word_indices: Vec<usize>,          // ← lines[].word_indices
}
```

### 4.3 OcrWord

```rust
pub struct OcrWord {
    pub text: String,                      // ← words[].text
    pub bounding_rect: OcrRect,            // ← words[].rect
    pub line_index: usize,                 // ← words[].line_index
}
```

### 4.4 OcrRect

```rust
pub struct OcrRect {
    pub x: i32,   // ← rect.x (四舍五入)
    pub y: i32,   // ← rect.y
    pub w: u32,   // ← rect.w (max(0))
    pub h: u32,   // ← rect.h (max(0))
}
```

### 4.5 return_word_box 能力验证

PaddleOCR 3.7 的 `return_word_box=True` 参数应能返回 word 级 box。spike 需验证：

1. word rect 是否非空、有限、在图像范围内
2. word rect 能否驱动现有阅读模式双向高亮
3. line rect 是否为该行 words 的 union（与现有 `rect_union` 一致）

**降级方案**: 如果 PP-OCRv6 不提供 word 级 rect（只有 line 级），则 word rect 降级为 line rect（已在 thin wrapper 中实现）。但这会影响阅读模式 word 级高亮精度。资格门只使用 native word rect 有效率，fallback 不计入资格门通过条件。

## 5. 协议版本

- `0.1.0` — spike 初始草案（使用 `deadline_ms`）
- `0.2.0` — spike 冻结草案（使用 `timeout_ms`，明确取消边界，增加 `native_word_boxes`/`fallback_word_boxes` 输出字段）
- `0.3.0` — 0.22.3 正式实现时版本（可能扩展字段）
