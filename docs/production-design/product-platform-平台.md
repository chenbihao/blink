# Blink 产品设计 · 平台扩展卷

> 扩展与智能化层：插件系统（含 surface 呈现权）、四域架构、意图路由、内部架构骨架、AI 能力方向。
>
> 配套：`product-interaction-交互.md`（交互/搜索）· `product-context-future-感知.md`（Context/隐私）· `product-principles-原则.md`（横切原则）· `phases/` 技术实现。

---

## 4. 插件系统

### 4.1 独立进程（安全隔离）

插件不加载 DLL，以独立进程运行，通过 stdio JSONL 通信。崩溃不影响 core。

### 4.2 插件即召回源

插件是搜索引擎的一路召回源，不是独立功能。用户输入 → 命中插件 trigger → 插件结果与普通搜索结果一起呈现。

### 4.3 呈现权模型（surface ownership）—— 核心产品决策

**核心认知：触发与呈现正交**

| 维度 | 回答的问题 | 谁决定 |
|---|---|---|
| **触发（match）** | query 算不算命中该插件 | keyword / regex + 是否带参 |
| **呈现（surface）** | 命中后如何占用 UI 返回区 | 插件 manifest 声明 + 命中强度 |

真正有价值的是**呈现权**：命中后插件在返回区占多大地盘、以什么形态呈现。这个区域**未来不一定是 item 列表**（`ai xxx` → 对话界面，`fs key` → 文件搜索专界面）。

#### 三态 surface

| surface | 路由影响 | UI 呈现 | 例子 |
|---|---|---|---|
| `inline` | 不独占，混排 | 普通 item，按分排序，与应用/文件并列 | 字典、单位换算 |
| `priority` | 不独占，**置顶** | 插件 item 排最前 + 其他引擎结果保留在下方 | 翻译（译文顶部，下面仍可搜应用） |
| `takeover` | **独占**，跳过其他引擎 | **接管整个返回区**（当前 = item 列表；未来 = 自定义 view） | `fs key` 文件搜索、`ai xxx` 对话 |

**`EngineTakeover` 变体**：本体 engine 独占（如 `剪贴板 xxx` 走 `ClipboardEngine`）与 `Takeover` 语义等价，区别在**执行分派**——插件走 JSONL IPC，本体 engine 直接调 sync engine。类型墙原则（与 `ExecArg` 一致）——把"这是本体不是插件"编译期钉死。Action trait 统一后（§7），两个变体的调用差异被 Action dispatch 屏蔽。

#### 空格 = surface 升级信号（auto 模式）

默认 `surface: auto` 的行为：

| 输入形态 | 例 | auto 解出的 surface | 理由 |
|---|---|---|---|
| 部分/模糊匹配 | `f` → fs | `inline` | 可能在搜别的，作候选 |
| 精确、无参 | `fs` | `priority` | 提示"文件搜索可用"置顶，但不屏蔽应用搜索 |
| keyword + 空格 + 参数 | `fs report` | `takeover` | 明确在喂参数，接管零损失 |

规避了短词 keyword（如 `note`）误屏蔽搜索结果（Notepad）的问题，又给了带参时的全接管体验。

#### view 字段——为未来留口子

takeover 不绑定 item 列表，协议留 `view` 字段：

| view | 含义 |
|---|---|
| `list`（默认） | item 列表 |
| `chat` | AI 对话界面（下方展开，流式） |
| `custom` | 插件自定义渲染 |

Route 与前端协议已预留 view 字段——0.9 接 AI 对话时不改路由层。

#### manifest 声明

```jsonc
"triggers": [
  { "type": "keyword", "keyword": "fs",
    "surface": "auto",          // auto = 无参 priority / 带参 takeover
    "takeover_view": "list" }
]
```

- 全局总闸：`surface_takeover_enabled=false` 时所有 takeover 降级 priority
- 首拼命中（弱信号）恒不独占（详见 [interaction §3.2](./product-interaction-交互.md#32-匹配策略)）

### 4.4 动作系统

**决策**：插件结果带 `action` 结构（Copy/Open/自定义），payload 透传到前端 `actions.js`。前端按 `action.kind + payload` 执行，不依赖后端硬编码行为。

**动作三种来源**，长期方向统一到 `Action` trait（见 §7）：

| 来源 | 触发 | 结果去向 | disable 存储 |
|---|---|---|---|
| **BuiltinAction** | keyword/context → results | mix 进 results，回车 `run_builtin_action` | `disabled_builtin_actions` |
| **Plugin** | keyword/context → results | mix 进 results，回车走 IPC | 插件 `enabled` |
| **ChordAction** | Alt+字母 → 独占屏幕/emit | 直接执行副作用 | `disabled_chord_actions` |

前端契约 `Action { kind, payload }` 是三源共用投影，后端 Action trait 收敛只改物理归一，前端契约不变。

### 4.5 权限模型（阶段判断）

**决策**：自用阶段完全不实现权限强制。manifest 的 `permissions` 字段不解析，插件直接拥有完整能力。权限强制（进程级隔离、用户确认）是产品化/对外分发的前置里程碑，1.0 前不做。

---

## 5. 意图引擎

### 5.0 四域架构（产品设计基座）

意图不是"用户想干什么"的语义分类，是**四个正交域协作产生的呈现权 + 执行权**：

```
┌─────────────────────────────────────────────────────────────┐
│  Awareness (环境感知) —— 抓 snapshot，纯数据不做判断         │
│                        ↓  唯一读它的层                        │
│  Suggestion (建议生产) —— Signal → Ghost 建议，待用户采纳    │
│                        ↓  ★ 信任边界：Tab/点击/打字才穿过    │
│  Routing    (路由决策) —— Query → Route，对 Awareness 无知   │
│                        ↓  只有用户显式选择才穿过              │
│  Execution  (执行)     —— UserExplicit 参数才真执行          │
└─────────────────────────────────────────────────────────────┘
```

**三条铁则**（架构强制，类型系统钉死）：

1. **呈现权 ≠ 执行权**：Routing 只出候选，Execution 需第二次交互
2. **参数注入必须显式**：`ExecArg::UserExplicit(String)` 类型墙，其他路径编译不过
3. **弱信号 pull 不 push**：Routing 无法读 Awareness，Context 只能通过 Suggestion 域影响

**违反流向的代码不应存在**——类型层拒绝 + 域接口收敛 + 单测按域拆分。

### 5.1 路由模型

```
用户输入 → RuleRouter（规则） → 命中 → 直接执行（确定性快速通道）
         → 未命中 → 前置过滤 → AI 路由（轻量模型，0.9） → 高置信 + 安全动作 → 执行
                                                          → 不确定 / 需多轮 → 回退
         → 全未命中 → Generic（全引擎召回）
```

**完全属于 Routing 域**（§5.0）。路由器都接受 `(query, history, ranking_hint)`，都不看 Awareness。AI 路由是内部策略升级，不改变域边界。

当前只实现 `RuleRouter`（keyword/regex），0.9 接入 AI 路由（轻量模型做意图分类 + 抽参）。**AI 是回退决策者，不是默认路径**——确定性命中永远走快速通道（不过 AI），保护 P0（详见 §6.4）。

> VectorRouter（向量语义匹配）原计划在 0.9，现已移出——LLM 路由更强，向量的归宿是后期 RAG / 记忆（0.20）。

### 5.2 Route 不是 Intent（意图分类推迟）

`route()` 返回 `Route`（呈现调度：Takeover/EngineTakeover/Mixed），而非语义枚举 `Intent`（OpenApp/Calculate/Plugin/Ai/Generic）。

**理由**：`Intent` 的真实消费者（AI 路由的语义分类）在 0.9 才出现。过早引入会造空抽象——路由层只需区分"怎么占用返回区"，不需要"用户想干什么"。等 0.9 有真实分类需求时，在 `Intent` 之上派生 `Route`。

### 5.3 keyword 匹配

- 精确命中 / 前缀带参（`keyword arg`）
- 中文 keyword 支持首拼输入（`tq` → "天气"）+ 完整拼音（`fanyi` → "翻译"）
- query 与 keyword 过同一归一化（小写 + `[原文, pinyin_full, pinyin_initials]` 三候选，首拼弱信号不独占）

### 5.4 Suggestion 域 —— AI 意图判定器的天然位子

Suggestion 域是唯一能读 Awareness 的层，也是**AI 意图判定器的落脚点**。三源共存：

- `KeywordProducer`（首拼 fy → fanyi、部分拼音 fan hello → fanyi hello）
- `ContextProducer`（选中英文 → 翻译建议 / 剪贴板 URL → 打开链接建议）
- **（0.9）`AIProducer`**（"帮我打开手边链接" → open_url 建议）

三源统一走 `SuggestionProducer` trait + `SuggestionArbiter` 竞争（见 §7）。产 `Suggestion { display, replacement, confidence, source, origin }`；0.9 加 AI 只需 `arbiter.register(...)` 一行。

**关键性质**：AI 永远只产 Suggestion，**永远不产 `Route.arg`**——类型系统禁止，守死"AI 有幻觉不能直接触发副作用"的产品原则。用户 Tab 是最后一道人类审核。

### 5.5 Surface Booster —— Context 影响首屏排序的合法路径

用户打"翻译" + 剪贴板恰好英文 → 翻译插件顶到首屏第一。这是便利性，但**不能让 Routing 偷读 Awareness**。

**机制**：Suggestion 产 Ghost 时同时产 `RankingHint { boost_plugin_id }`，`SuggestionArbiter` 独立通道汇总给 SearchService，下一轮 `route()` 把 hint 作为**排序梯标**传入——只影响 surface 排序，不影响 arg / 候选集。

- Suggestion 域读 Awareness → 产 hint
- Routing 域接受 hint 作排序输入 → 不读 Awareness
- 便利性保留，域边界不破

**已知限制**：跨轮反馈滞后一轮。0.9 AI 异步化后此机制需改为「异步 hint 到达触发增量重排」。

---

## 6. 护城河定位 + 统一 Tool 架构（0.9+ 智能化方向）

> 0.9+ 完整演进见 [phases/0.9-ai-layer.md](./phases/0.9-ai-layer.md) / [0.10-voice-agent.md](./phases/0.10-voice-agent.md) / [0.11-plugin-ai-toolchain.md](./phases/0.11-plugin-ai-toolchain.md)。本节只留产品级铁则。

### 6.1 护城河：感知 + 执行，不是推理

Blink 不是「内置 AI 的启动器」，是**「本地 AI 的感知与执行层」**。推理大脑可插拔（任意 Provider / Agent），护城河是 Web AI 客户端够不着的三样：

| 护城河 | 说明 | Web AI 能做到吗 |
|---|---|---|
| **全局感知** | Alt+Space 全屏全局唤起 + UIA 划词 + 剪贴板 + 前台应用 +（0.10）语音 | ❌ |
| **本地执行** | 打开文件 / 运行命令 / 资源管理器定位 / 截图（系统权限） | ❌ |
| **速度** | 唤起 &lt; 50ms、首个结果 &lt; 20ms | ❌ |

**产品铁则**：任何让 Blink 变成「纯对话壳子」的设计都要拒绝。AI 是消费者（调 Blink 的 tool）兼生产者（产 Suggestion），Blink 是身体。大脑可换，身体不可换。

### 6.2 能力体系：Capability（开放）+ Action（本地执行）

> **0.14 能力协议重构后的边界**。旧版「一切皆 tool」已被推翻——详见 [phases/0.14](./phases/0.14-capability-protocol-refactor.md)。

Blink 的能力分两层，**边界用类型系统钉死**：

| | **Capability**（开放能力） | **Action**（本地执行） |
|---|---|---|
| 定位 | 面向所有调用方（AI / CLI / MCP / 主窗口） | 主窗口 / chord 本地执行域 |
| 进 AI tool 池 | ✅ **唯一合法形态**（经 `CapabilityTool` 适配）| ❌ 永不（0.14 删 `ActionTool` 适配器）|
| 语义 | 纯数据能力（取数/计算/翻译/打开），返回 `CapabilityResult` | 已执行副作用描述（Copy/Open/Emit/Nop）|
| 例子 | search_files / translate / capture_screen / open_url | lock / shutdown / open_settings / chord 动作 |

**核心铁则**：AI 永远只通过 `CapabilityTool` 调能力。`open_url` / `open_path` / `reveal_in_explorer` 这三个 AI 常用的从 Action 提升为 Capability；`lock` / `shutdown` 等不可逆操作留在 Action，AI 看不到，避免安全隐患。

**Capability 协议分层**（0.14）：插件只吐纯 `data`，投影规则（pointer / desc / actions）上移到 manifest 做代理——翻译插件返回 `data: "你好"` + manifest 配 `desc: "译文"` 即可，零 UI 代码。四出口（主窗口 AI / 对话窗口 AI / CLI / MCP）共用一套 canonical 投影引擎。

- 细节见 [phases/0.14-capability-protocol-refactor.md](./phases/0.14-capability-protocol-refactor.md)

### 6.3 Provider 多档（能力供应商）

Provider 不只是聊天 API，是**能力供应商**：LLM（`chat`/`chat_stream`）/ STT（`transcribe`，0.10）/ embedding（留 RAG）。一个 Provider 可只供一项（本地 whisper 只供 STT），也可全供（OpenAI）。

三档配置（参考 Obsidian YOLO 式交互）：**路由模型**（意图分类 + 抽参，快、便宜、可本地）/ **轻量模型**（日常单轮）/ **主模型**（多步推理）。档位空缺自动降级。

> **不照搬 YOLO 全自动执行**：危险动作必确认，四域信任边界不让步。细节见 [phases/0.9 §四](./phases/0.9-ai-layer.md)。

### 6.4 两条路径铁则 + AI 反馈铁则

- **确定性路径**（&lt; 1s）：命中规则 → 直接执行，**不过 AI**，保护 P0
- **模糊意图路径**：未命中才走 AI 路由（AI 永远是**回退决策者**，不是默认路径）
- **AI 反馈铁则**：AI 调用期间主窗口必有 loading / 过渡，**不能让用户死等**——不能把延迟从「唤起」挪到「路由」，那是体验等价退化

### 6.5 演进时间线

| 版本 | 主题 |
|---|---|
| **0.9** | Agent 地基：统一 tool 架构 + Provider + **纯文本**闭环（零语音） |
| **0.10** | 语音输入：STT + 语音打字（G1/G2）+ 伪流式 VAD 切句 + FunASR 本地化 + 文本注入（架构不变，只加感知层） |
| **0.11** | 插件通信契约重设计 + AI 调用插件链路完善 + 截图标注增强 + OCR word 级链路 + 阅读模式 + 翻译衔接 + 水印独立图层 + 图上翻译 |
| **0.12** | AI 能力架构搭建：对话窗口 / 对话机制 / DB 四层拆分 / Provider 模型统一管理 / ollama+lmstudio |
| **0.13** | 能力扩展基础版 + 开放：MCP client / MCP server（护城河）/ CLI 化 / token-aware context 压缩 / 记忆 FTS5 召回 / Skill 约定式 |
| **0.14** | **能力协议重构**：Capability/Action 边界钉死 + Cap 协议分层（插件只吐 data + manifest 投影）+ 四出口投影引擎收敛 |
| **0.20** | 能力扩展向量版：zvec 向量基础设施 / 记忆向量召回（混合检索）/ RAG 知识库 |

**解耦智慧**：先验证大脑（0.9 文本闭环），再加感官（0.10 语音）。0.12-0.14 是 AI 能力架构的「搭骨架 → 扩展 → 收敛重构」三步。

### 6.6 AI 对话 = takeover view（0.12 已完成）

AI 对话不是 item 列表，而是 `view: chat` 的 takeover 区域——独立对话窗口（Alt+Q chord 唤起），走流式 JSONL。这是 §4.3 view 字段预留的目的。0.12 已落地：独立 chat 窗口 + 流式 Markdown + 多对话管理 + SQLite 持久化 memory + Tool loop。

---

## 7. 内部架构骨架（三个统一入口）

四域是**逻辑规范**，三个统一入口 trait 让四域成为**物理骨架**。0.14 后 AI 能力的接入点从 Action 迁到 Capability。

| 域 | 统一入口 | AI 如何接入 |
|---|---|---|
| **Execution（本地）** | `Action` trait + `ActionOutcome` | **AI 不接入**（0.14 后 AI 走 Capability，Action 退回纯本地执行） |
| **Capability（开放）** | `Capability` trait + `CapabilityResult` + `CapabilityTool` 适配 | AI 产 tool-call → `CapabilityTool::call()` → `cap.invoke()` → `CapabilityResult` |
| **Suggestion** | `SuggestionProducer` trait + `SuggestionArbiter` | `AIProducer` register 到 arbiter，三源竞争 |
| **配置** | `ConfigStore<T>` 泛型 + `blink://config-changed` 广播 | AI Provider 配置 `impl ConfigKey for AIConfig`，前端零脚手架 |

### 7.1 Action trait —— 本地副作用的统一入口（0.14 后不再接 AI）

```rust
pub trait Action: Send + Sync {
    fn id(&self) -> &str;
    async fn execute(&self, cx: &ActionContext) -> Result<ActionOutcome, ExecError>;
}

// 0.13.7 后 ActionOutcome 只剩副作用描述（Items 变体已删，迁入 Capability）
pub enum ActionOutcome {
    Copy { text: String, hit_id: Option<String> },
    Open { path: String },
    Emit { event: String, payload: serde_json::Value },  // Chord 副作用统一走这
    Nop,
}
```

三种来源（0.14 后移除了"AI Action"来源）：
- **BuiltinAction** → 每个 kind 一个 struct，各自 `impl Action`（lock/shutdown/open_settings 等 9 个，服务主窗口/chord）
- **ChordAction** → 直接实现 `Action`（副作用走 `Emit`）

> **注意**：`open_url` / `open_path` / `reveal_in_explorer` 在 0.14 已从 Action 提升为 Capability，AI 可调用。其余 9 个 Action 永不进 AI tool 池。

### 7.1b Capability trait —— 开放能力的统一入口（AI 唯一调用形态）

```rust
pub trait Capability: Send + Sync {
    fn id(&self) -> &str;
    fn schema(&self) -> CapabilitySchema;        // 送 LLM 的 tool schema
    fn danger_class(&self) -> DangerClass;        // Safe / Dangerous
    async fn invoke(&self, args: Value, ctx: &InvokeContext) -> Result<CapabilityResult, CapabilityError>;
}

// 四变体覆盖所有能力返回（0.14 重定义字段语义）
pub enum CapabilityResult {
    Text { content: String, desc: Option<String> },
    Items { items: Vec<ItemResult> },
    Blob { mime: String, bytes: Vec<u8>, desc: Option<String> },
    Done { summary: String },
}
```

AI 进 tool 池的唯一适配器是 `CapabilityTool`（impl rig `ToolDyn`）。0.14 删除了 `ActionTool`——AI 永远无法直接调 Action。详见 [§6.2](#62-能力体系capability开放--action本地执行) 和 [phases/0.14](./phases/0.14-capability-protocol-refactor.md)。

### 7.2 SuggestionProducer + Arbiter —— 一切建议只从这一处出

```rust
pub trait SuggestionProducer: Send + Sync {
    fn source(&self) -> SuggestionSource;
    fn produce(&self, query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion>;
}

pub struct SuggestionArbiter {
    producers: Vec<Arc<dyn SuggestionProducer>>,
}
// best(query, snapshot) → (Option<Suggestion>, Option<RankingHint>)
```

三源竞争走同一路径；`RankingHint` 独立返回（从 `Suggestion` 结构剥离），Suggestion → Routing 单向反馈通道。

### 7.3 ConfigStore\<T\> —— 一切配置只从这一处出

`AppConfig` 拆 6 片：

| 分片 | KV key | 内容 |
|---|---|---|
| `HotkeyConfig` | `app.hotkey` | 快捷键 / tap 阈值 / grace period |
| `AppearanceConfig` | `app.appearance` | 主题 / 语言 / 自启 |
| `SearchConfig` | `app.search` | 结果条数 / 分页 / 历史开关 / takeover 总闸 |
| `SuggestionConfig` | `app.suggestion` | Autosuggest 开关 / 阈值 / Tab 键 |
| `ChordConfig` | `app.chord` | Chord 总开关 / 提示可见性 |
| `DisableConfig` | `app.disable` | 三个黑名单（builtin/context/chord） |

`DisableConfig` 单独一片：后续加新黑名单类型（如 `disabled_ai_providers`）有归宿，不用穿透到其他片。

**前端 API**：泛型 `get_config(key) / set_config(key, value)` 命令 + `frontend/js/config-keys.js` 维护 key 常量表 + `blink://config-changed` 广播（各模块按 key 订阅）。取代散落 20+ 个 `update_*` 专用命令。

### 7.4 与 AI 的对齐（0.14 后）

四个入口就是 AI 的接入点：
- AI 产的 **tool-call** → `CapabilityTool` → `Capability::invoke()`（**唯一调用形态**，0.14 后 Action 不再接 AI）
- AI 判定的 **建议** → `SuggestionProducer`
- AI 的 **配置** → `ConfigStore`

**四域信任边界依旧成立**：AI 只能产 Suggestion / tool-call 候选，不能构造 `ExecArg::UserExplicit`。用户 Tab 采纳后才穿过 Suggestion → Execution 边界。危险 Capability（如读隐私数据）AI 调用时需用户确认（`DangerClass::Dangerous` 或 `sensitive` 标记）。
