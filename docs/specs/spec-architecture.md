# 架构与领域规范

> **本文是 Blink 架构的 single source of truth**——分层、四域、统一入口、能力边界、协议、信任边界、AI 接入点、护城河演进,全在这一处。
>
> 配套:`../product.md`(产品决策:为什么这么设计)· `../phases/`(每版实现)。同层 spec:`spec-frontend.md` / `spec-backend.md` / `spec-phase.md`。
>
> **读法**:本文只留**最终决策**与**结构骨架**(trait 签名只放声明 + 一句注释)。实现细节、踩坑、验收在 `../phases/`;铁则的"为什么不可妥协"在 `../product.md`。

---

## §A0 导读

Blink 的架构由两层正交的骨架撑起:

| 骨架 | 性质 | 把它钉成物理实体的东西 |
|---|---|---|
| **分层架构**(§A1) | 物理——模块怎么摆、依赖往哪指 | `src/{app,domain,infra,cli}` 四目录 + 依赖方向约束 |
| **四域架构**(§A2) | 逻辑——数据/控制流怎么走、信任边界在哪 | 三个统一入口 trait(§A3) + `ExecArg` 类型墙(§A7) |

之上叠加能力体系(§A4-§A6,0.14 重构定稿)、信任边界(§A7)、AI 接入约定(§A8)、护城河与演进(§A9)。

新人路径:**§A1 分层 → §A2 四域 → §A3 三入口 → §A4 能力边界 → §A5/§A6 协议与投影 → §A7 信任边界**。读完这六节就能定位任何代码"它该在哪一层、属于哪个域、走哪个入口"。

---

## §A1 分层架构总览

Blink 源码分四层目录,依赖**只准向下**:

```
┌──────────────────────────────────────────────────────────────┐
│  cli/        ← 自身 CLI 化(mcp-server / search / run / chat)  │  最薄,可调 domain + infra
├──────────────────────────────────────────────────────────────┤
│  app/        ← 应用层:Tauri IPC 入口 + 各服务 wiring + 配置    │  依赖 domain + infra
│              · commands(Tauri command 入口,0.15 按域拆分)      │
│              · config / ai_config / stt_config(0.15 下沉 domain)│
│              · voice(语音管线编排)                            │
├──────────────────────────────────────────────────────────────┤
│  domain/     ← 业务域:四域逻辑 + 能力协议(框架无关)           │  依赖 infra,不依赖 tauri(0.15 收敛)
│              · context/  intent/  search/  execution/         │
│              · plugin/  chord/  ai/  stt/  capability/        │
├──────────────────────────────────────────────────────────────┤
│  infra/      ← 基础设施:平台调用 + 数据 + 工具                │  最底层,不反向依赖上面
│              · platform/(hotkey/window/clipboard/screenshot…) │
│              · data/(SQLite 四库)  · utils/(logging/text/perf) │
└──────────────────────────────────────────────────────────────┘
        依赖方向:只准向下。domain 不 use tauri,infra 不 use app/domain。
```

**各层职责**:

| 层 | 职责 | 0.15 收敛目标 |
|---|---|---|
| `cli` | `blink mcp-server/search/run/capabilities/config/chat`,clap + 最小 Tauri app 无 GUI(0.13.5) | 已达标 |
| `app` | Tauri IPC 入口 + 服务 wiring + 配置门面 | 配置类型下沉 domain/`config` 域;commands 巨石按域拆分 |
| `domain` | 四域业务逻辑 + 能力协议,**框架无关** | 去 `use tauri`(领域事件 + 依赖注入);Win32 调用收进 infra |
| `infra` | 平台抽象(`mod.rs` 接口 + `windows.rs` 实现)+ SQLite 四库 + 工具 | 收纳从 domain 泄漏的 Win32 调用(icon/shell/lock/OCR) |

**关键约束**(0.15 正在收敛,详见 [phases/0.15](../phases/0.15-architecture-cleanup.md)):

- `domain/` **不 `use tauri::`**——发领域事件,app 层桥接成 `app_handle.emit()`;取状态走依赖注入而非 `app_handle.state::<T>()`
- `domain/` **不 `use windows::`**——所有 Win32 走 `infra/platform/` 的 trait 抽象
- `infra/`、`domain/` **不反向 `use crate::app::config`**——配置类型归 `domain/config` 域,方向才对

**数据存储**(SQLite 四库独立写锁,`%APPDATA%\blink\`):

| 库 | 内容 |
|---|---|
| `blink_config.db` | 配置 KV(`AppConfig` 6 分片 + `AIConfig` 第7 + `SttConfig` 第8 + 各引擎/插件配置)。未来跨机同步只同步此库 |
| `blink_history.db` | 启动历史 + 剪贴板历史 |
| `blink_ai.db` | AI 工具审计(30天清理 + 上限 10000)+ conversations/messages(对话记忆) |
| `blink_cache.db` | 性能统计(高频写)+ 图标缓存(BLOB) |

> 实现细节见 [phases/0.15](../phases/0.15-architecture-cleanup.md)(分层清理目标态)与 [phases/0.12 §基础设施](../phases/0.12-ai-ecosystem.md)(DB 四层拆分落地)。

---

## §A2 四域逻辑架构(设计基座)

意图不是"用户想干什么"的语义分类,是**四个正交域协作产生的呈现权 + 执行权**:

```
┌─────────────────────────────────────────────────────────────┐
│  Awareness (环境感知) —— 抓 snapshot,纯数据不做判断         │
│                        ↓  唯一读它的层                        │
│  Suggestion (建议生产) —— Signal → Ghost 建议,待用户采纳    │
│                        ↓  ★ 信任边界:Tab/点击/打字才穿过    │
│  Routing    (路由决策) —— Query → Route,对 Awareness 无知   │
│                        ↓  只有用户显式选择才穿过              │
│  Execution  (执行)     —— UserExplicit 参数才真执行          │
└─────────────────────────────────────────────────────────────┘
```

**三条铁则**(架构强制,类型系统钉死):

1. **呈现权 ≠ 执行权**:Routing 只出候选,Execution 需第二次交互
2. **参数注入必须显式**:`ExecArg::UserExplicit(String)` 类型墙,其他路径编译不过
3. **弱信号 pull 不 push**:Routing 无法读 Awareness,Context 只能通过 Suggestion 域影响

**违反流向的代码不应存在**——类型层拒绝 + 域接口收敛 + 单测按域拆分。

> 每个域的职责落点:`Awareness` 见 [感知卷 §7](../product.md);`Suggestion`/`Routing` 见 [平台卷 §5](../product.md);`Execution` 见 §A3。落地实现(类型墙、route() 断 Awareness)见 [phases/0.8 §五/§八](../phases/0.8-context-interaction.md)。

---

## §A3 三个统一入口(物理骨架)

四域是**逻辑规范**,三个统一入口 trait 让四域成为**物理骨架**。0.14 后 AI 能力的接入点从 Action 迁到 Capability,于是实际是**四个入口**:

| 域 | 统一入口 | AI 如何接入 |
|---|---|---|
| **Execution(本地)** | `Action` trait + `ActionOutcome` | **AI 不接入**(0.14 后 AI 走 Capability,Action 退回纯本地执行) |
| **Capability(开放)** | `Capability` trait + `CapabilityResult` + `CapabilityTool` 适配 | AI 产 tool-call → `CapabilityTool::call()` → `cap.invoke()` → `CapabilityResult` |
| **Suggestion** | `SuggestionProducer` trait + `SuggestionArbiter` | `AIProducer` register 到 arbiter,三源竞争 |
| **配置** | `ConfigStore<T>` 泛型 + `blink://config-changed` 广播 | AI Provider 配置 `impl ConfigKey for AIConfig`,前端零脚手架 |

### §A3.1 Action trait —— 本地副作用的统一入口(0.14 后不再接 AI)

```rust
pub trait Action: Send + Sync {
    fn id(&self) -> &str;
    async fn execute(&self, cx: &ActionContext) -> Result<ActionOutcome, ExecError>;
}

// 0.13.7 后 ActionOutcome 只剩副作用描述(Items 变体已删,迁入 Capability)
pub enum ActionOutcome {
    Copy { text: String, hit_id: Option<String> },
    Open { path: String },
    Emit { event: String, payload: serde_json::Value },  // Chord 副作用统一走这
    Nop,
}
```

两种来源(0.14 后移除了"AI Action"来源):
- **BuiltinAction** → 每个 kind 一个 struct,各自 `impl Action`(lock/shutdown/open_settings 等 9 个,服务主窗口/chord)
- **ChordAction** → 直接实现 `Action`(副作用走 `Emit`,是 Action 的 supertrait 而非平行 trait)

> **注意**:`open_url` / `open_path` / `reveal_in_explorer` 在 0.14 已从 Action 提升为 Capability(§A4),AI 可调用。其余 9 个 Action 永不进 AI tool 池。

### §A3.2 Capability trait —— 开放能力的统一入口(AI 唯一调用形态)

```rust
pub trait Capability: Send + Sync {
    fn id(&self) -> &str;
    fn schema(&self) -> CapabilitySchema;        // 送 LLM 的 tool schema
    fn danger_class(&self) -> DangerClass;        // Safe / Dangerous
    async fn invoke(&self, args: Value, ctx: &InvokeContext) -> Result<CapabilityResult, CapabilityError>;
}

// 四变体覆盖所有能力返回(0.14 重定义字段语义)
pub enum CapabilityResult {
    Text { content: String, desc: Option<String> },
    Items { items: Vec<ItemResult> },
    Blob { mime: String, bytes: Vec<u8>, desc: Option<String> },
    Done { summary: String },
}
```

AI 进 tool 池的唯一适配器是 `CapabilityTool`(`impl rig ToolDyn`)。0.14 删除了 `ActionTool`——AI 永远无法直接调 Action。

### §A3.3 SuggestionProducer + Arbiter —— 一切建议只从这一处出

```rust
pub trait SuggestionProducer: Send + Sync {
    fn source(&self) -> SuggestionSource;
    fn produce(&self, query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion>;
}

pub struct SuggestionArbiter { producers: Vec<Arc<dyn SuggestionProducer>> }
// best(query, snapshot) → (Option<Suggestion>, Option<RankingHint>)
```

三源竞争走同一路径;`RankingHint` 独立返回(从 `Suggestion` 结构剥离),Suggestion → Routing 单向反馈通道。

### §A3.4 ConfigStore\<T\> —— 一切配置只从这一处出

`AppConfig` 拆 8 片:

| 分片 | KV key | 内容 |
|---|---|---|
| `HotkeyConfig` | `app.hotkey` | 快捷键 / tap 阈值 / grace period |
| `AppearanceConfig` | `app.appearance` | 主题 / 语言 / 自启 |
| `SearchConfig` | `app.search` | 结果条数 / 分页 / 历史开关 / takeover 总闸 |
| `SuggestionConfig` | `app.suggestion` | Autosuggest 开关 / 阈值 / Tab 键 |
| `ChordConfig` | `app.chord` | Chord 总开关 / 提示可见性 |
| `DisableConfig` | `app.disable` | 三个黑名单(builtin/context/chord) |
| `AIConfig` | `ai.*` | Provider 多档 + 对话配置(第7片,0.12) |
| `SttConfig` | `stt.*` | 语音引擎配置(第8片,0.10) |

`DisableConfig` 单独一片:后续加新黑名单类型(如 `disabled_ai_providers`)有归宿,不用穿透到其他片。

**前端 API**:泛型 `get_config(key) / set_config(key, value)` 命令 + `frontend/js/config-keys.js` 维护 key 常量表 + `blink://config-changed` 广播(各模块按 key 订阅)。取代散落 20+ 个 `update_*` 专用命令。

> 三个入口的落地(0.8.6 架构固化)见 [phases/0.8 §八](../phases/0.8-context-interaction.md)。

---

## §A4 能力体系边界(Capability / Action 钉死)

> **0.14 能力协议重构后的边界**。旧版"一切皆 tool"已被推翻——详见 [phases/0.14](../phases/0.14-capability-protocol-refactor.md)。

Blink 的能力分两层,**边界用类型系统钉死**:

| | **Capability**(开放能力) | **Action**(本地执行) |
|---|---|---|
| 定位 | 面向所有调用方(AI / CLI / MCP / 主窗口) | 主窗口 / chord 本地执行域 |
| 进 AI tool 池 | ✅ **唯一合法形态**(经 `CapabilityTool` 适配)| ❌ 永不(0.14 删 `ActionTool` 适配器)|
| 语义 | 纯数据能力(取数/计算/翻译/打开),返回 `CapabilityResult` | 已执行副作用描述(Copy/Open/Emit/Nop)|
| 例子 | search_files / translate / capture_screen / open_url | lock / shutdown / open_settings / chord 动作 |

**核心铁则**:AI 永远只通过 `CapabilityTool` 调能力。`open_url` / `open_path` / `reveal_in_explorer` 这三个 AI 常用的从 Action 提升为 Capability;`lock` / `shutdown` 等不可逆操作留在 Action,AI 看不到,避免安全隐患。

**边界用类型系统钉死**——删除 `ActionTool` 适配器后,AI 该不该调某个能力的决策从"运行时每个 Action 自己标 danger_class"前置成"编译期只有 Capability 才能进 tool 池"。

**不合并 Capability 与 Action 的理由**:四域架构(§A2)是基座,Action trait 承载 Execution 域"已执行副作用"语义,与 Capability"纯数据能力"是不同的事;且 lock/shutdown 等本就不该让 AI 直接调。0.13.7 删 `ActionOutcome::Items` 让 Action 回归纯副作用,说明架构**正朝"Action=副作用专用"收敛,而非合并**。

> 价值论证(为什么这条边界不可妥协)见 [principles §13.4](../product.md)。落地(删 ActionTool、三个提升为 Capability)见 [phases/0.14 §二](../phases/0.14-capability-protocol-refactor.md)。

---

## §A5 Capability 协议分层(插件只吐纯 data)

> 0.14 引入的两层解耦。核心思想:**展示和投影是配置出来的,不是代码写死的**。

```
┌─────────────────────────────────────────────────────────────┐
│  层1: manifest(投影规则,相对静态)                          │
│   data_shape / pointer / desc / actions                      │
│   ← "怎么展示 / 怎么投影"是配置出来的                          │
├─────────────────────────────────────────────────────────────┤
│  层2: 插件运行时输出(纯数据,绝对纯净)                      │
│   data = 纯净 JSON(译文字符串 / IP 列表 / 天气对象)          │
│   ← 插件开发者只关心"返回正确的数据"                          │
└─────────────────────────────────────────────────────────────┘
         ↓ 投影引擎(§A6)用 manifest 规则把 data 投影成各出口形态
    AI 读 data / 主窗口读 pointer 指向的值 + desc / CLI 读 data
```

**精髓**:翻译插件从返回 `data: "你好"` → manifest 一行 `desc: "译文"` 就够了,零 UI 代码。

### §A5.1 关键设计点

- **JSONPath 取值**:用 `jsonpath-rust` crate(纯 Rust,~30KB)。单值/数组通配/嵌套对象/过滤都覆盖,后续向量检索、RAG chunk 提取可复用。不用自造 dialect。
- **desc 三来源优先级**:① manifest `desc_pointer` 指定 data 某字段(动态)→ ② manifest 静态字符串 → ③ 都没有则 None(不展示)。manifest **不做格式化**——需 `format_size(bytes)` 这种时由能力单元自己在 data 里算好。
- **双轨制**:轨道 A(manifest 投影,简单返回)与轨道 B(直接吐完整 `CapabilityResult`,复杂返回/格式化/builtin capability)都合法。

### §A5.2 新协议结构

```rust
/// 插件运行时只构造这个(轨道 A)
pub struct PluginRawResult {
    pub data: serde_json::Value,   // 纯数据,零展示逻辑
    pub error: Option<String>,
}

pub enum CapabilityResult {
    Text { content: String, desc: Option<String> },
    Items { items: Vec<ItemResult> },
    Blob { mime: String, bytes: Vec<u8>, desc: Option<String> },
    Done { summary: String },
}

pub struct ItemResult {
    pub data: serde_json::Value,       // 给 AI/CLI/MCP 的语义数据(自解释)
    pub desc: Option<String>,          // 给主窗口展示的可选副标题
    pub actions: Vec<ItemAction>,      // copy / open_file / open_url / ...
}

pub enum ItemAction {
    Copy { pointer: Option<String> },
    OpenFile { pointer: Option<String> },
    OpenUrl { pointer: Option<String> },
    Reveal { pointer: Option<String> },
}
```

**对比旧 `ItemResult` 的关键变化**:删 `title`/`subtitle`/`score`(主窗口展示概念,不该在协议层);`data` 取代 `payload`;主标题(旧 title)由前端从 `data` 派生(投影规则见 §A6)。

### §A5.3 多 action 交互约定

manifest 里 `actions` 数组**顺序即优先级**:

| 操作 | 行为 |
|---|---|
| 回车 | 执行 `actions[0]`(首个) |
| 右键 | 展开全部 actions 供选择 |

插件作者把最常用动作放第一个。空 `actions` + 纯标量 data 时,主窗口隐式提供 copy(**展示层 fallback,非协议层派生**)。

> 完整协议结构、迁移策略(一次性切换,0.x 插件全自研无兼容层)、选型理由见 [phases/0.14 §三/§四/§八](../phases/0.14-capability-protocol-refactor.md)。

---

## §A6 投影引擎(四出口共用)

能力结果(`CapabilityResult`)有 4 个消费出口,**共用一个投影引擎**,不再各自手写:

| 出口 | 读什么 | 投影规则 |
|---|---|---|
| **AI 出口**(主窗口回流 / 对话窗口) | `data`(纯 JSON) | 不读 desc,AI 只需语义数据 |
| **主窗口前端** | `data` 派生主标题 + `desc` + `actions` | 投影成 AppEntry |
| **CLI 出口** | `data` + `desc` | 文本展示 |
| **MCP 出口** | `data`(纯 JSON 给外部 agent) | 同 AI,只读 data |

```rust
/// 用 ProjectionRule 把 PluginRawResult 投影成 CapabilityResult(轨道 A)
pub fn project(raw: &PluginRawResult, rule: &ProjectionRule) -> CapabilityResult { ... }
```

**主窗口前端的 fallback 派生**——主标题(旧 `title`)由前端从 `data` 派生:纯字符串→直接用,纯数字→to_string,对象→JSON 串兜底。复杂对象主标题(如 search_files 的文件名)走轨道 B(builtin 直接构造好 ItemResult,data 带格式化字段)。

**收敛后消除的重复**:
- Blob 摘要逻辑(旧 4 份逐字重复)→ 收进 `CapabilityResult::blob_summary()` 一个方法
- CLI match 四变体 / MCP `result_to_call_tool_result` / 对话历史加载内联 match → 全改调 canonical 投影
- 修 MCP 投影 Items 的 score 漂移(旧裸 `serde_json::to_string` 含 score,AI 路径去 score)

> 落地与验收见 [phases/0.14 §五](../phases/0.14-capability-protocol-refactor.md)。

---

## §A7 信任边界与 ExecArg 类型墙

四域架构(§A2)的信任边界,在 AI 时代有两层含义:**跨域的数据流向约束** + **AI 的授权粒度分层**。

### §A7.1 跨域数据流向(铁则2、3)

- Routing **不能**读 Awareness——`route()` 签名无 `snapshot` 参数,编译级保证
- 参数注入必须显式——`ExecArg::UserExplicit(String)` 类型墙,其他路径编译不过
- Context 只能通过 Suggestion 域影响 Routing(产 `RankingHint` 作排序梯标,不改候选集)

### §A7.2 AI 授权粒度按交互模式分层

| 模式 | 授权粒度 | AI 能做什么 | 危险动作 |
|---|---|---|---|
| **主窗口模式** | 每次工具调用 | 只产 Suggestion / tool-call 候选,`ExecArg::UserExplicit` 类型墙每次检查 | 高置信也必须 Tab/确认 |
| **对话窗口模式**(0.12.1 Alt+Q) | 整个会话 | 显式授权后允许 `AgentBuilder` 自主 tool loop | **依然必确认**(独立于粒度扩展) |

**关键**:危险动作确认**独立于交互模式**——即使对话窗口 AI 自主决定要调 `Dangerous` Capability,依然走内嵌卡片人机确认(不用 Modal)。`DangerClass::Dangerous` 或 `sensitive` 标记是硬约束,不让授权粒度扩展绕过。

**AI 永远只产 Suggestion,永远不产 `Route.arg`**——类型系统禁止,守死"AI 有幻觉不能直接触发副作用"。用户 Tab 是最后一道人类审核。

> 价值论证见 [principles §13.4](../product.md)。主窗口/Agent 窗口两种模式的落地形态见 [phases/0.9 §4.3](../phases/0.9-ai-layer.md)。

---

## §A8 AI 接入点总览(0.14 后)

四个入口就是 AI 的接入点:

| AI 产出 | 接入点 | 路径 |
|---|---|---|
| **tool-call** | Capability | AI → `CapabilityTool` → `Capability::invoke()` → `CapabilityResult` → 投影(§A6)给四出口 |
| **建议(Suggestion)** | Suggestion | `AIProducer` register 到 `SuggestionArbiter`,与 Keyword/Context 三源竞争 |
| **配置** | ConfigStore | Provider 配置 `impl ConfigKey for AIConfig` |
| ~~tool-call(副作用)~~ | ~~Action~~ | **0.14 后此路不通**——删 `ActionTool`,AI 永不直接调 Action |

**四域信任边界依旧成立**:AI 只能产 Suggestion / tool-call 候选,不能构造 `ExecArg::UserExplicit`。用户 Tab 采纳后才穿过 Suggestion → Execution 边界。危险 Capability AI 调用时需用户确认(§A7.2)。

**两条路径铁则**:
- **确定性路径**(< 1s):命中规则 → 直接执行,**不过 AI**,保护 P0
- **模糊意图路径**:未命中才走 AI 路由(AI 永远是**回退决策者**,不是默认路径)
- **AI 反馈铁则**:AI 调用期间主窗口必有 loading/过渡,不能让用户死等

**AI 对话 = takeover view**(0.12 已完成):AI 对话不是 item 列表,而是 `view: chat` 的 takeover 区域——独立对话窗口(Alt+Q),走流式 JSONL。这是 surface 模型 `view` 字段预留的目的。

---

## §A9 护城河与演进

### §A9.1 护城河:感知 + 执行,不是推理

Blink 不是「内置 AI 的启动器」,是**「本地 AI 的感知与执行层」**。推理大脑可插拔(任意 Provider / Agent),护城河是 Web AI 客户端够不着的三样:

| 护城河 | 说明 | Web AI 能做到吗 |
|---|---|---|
| **全局感知** | Alt+Space 全屏全局唤起 + UIA 划词 + 剪贴板 + 前台应用 + 语音 | ❌ |
| **本地执行** | 打开文件 / 运行命令 / 资源管理器定位 / 截图(系统权限) | ❌ |
| **速度** | 唤起 < 50ms、首个结果 < 20ms | ❌ |

**产品铁则**:任何让 Blink 变成「纯对话壳子」的设计都要拒绝。AI 是消费者(调 Blink 的 tool)兼生产者(产 Suggestion),Blink 是身体。大脑可换,身体不可换。

**架构铁则**:**Agent 后端坚持 rig-core 自建,不用 opencode/pi 当执行端**——它们是和 Blink 同层的 agent 产品(非依赖),外包执行端会违反「不做 AI 运行时」边界、报废 0.12-0.20 架构投入。现成 agent 的正确用法是当 subagent/MCP server,不当后端。详见 [§A10](#§a10-adr-001agent-后端策略附录)。

### §A9.2 Provider 多档(能力供应商)

Provider 不只是聊天 API,是**能力供应商**:LLM(`chat`/`chat_stream`)/ STT(`transcribe`)/ embedding(留 RAG)。一个 Provider 可只供一项(本地 whisper 只供 STT),也可全供(OpenAI)。

三档配置:**路由模型**(意图分类 + 抽参,快、便宜、可本地)/ **轻量模型**(日常单轮)/ **主模型**(多步推理)。档位空缺自动降级。

> **不照搬全自动执行**:危险动作必确认,四域信任边界不让让步。

### §A9.3 能力体系演进时间线

| 版本 | 主题 | 架构动作 |
|---|---|---|
| **0.9.7** | Capability 能力协议层诞生 | 原子能力 + 统一声明/返回 + inventory 注册 + 截图/剪贴板拆解 + 接 AI tool 池 |
| **0.12** | AI 能力架构搭骨架 | 对话窗口 / DB 四层拆分 / Provider 模型统一 / Tool 适配层 / CapabilityRegistry 动态注册 |
| **0.13** | 能力扩展(基础版 + 开放) | MCP client/server / CLI 化 / token-aware 压缩 / 记忆 FTS5 召回 / Skill 约定式 / 0.13.7 收敛(P3 投影剔 score + 插件 Action→Capability 迁移,删 `ActionOutcome::Items`) |
| **0.14** | 能力协议重构(收敛) | Capability/Action 边界钉死(删 `ActionTool`) + Cap 协议分层(§A5) + 四出口投影引擎收敛(§A6) |
| **0.15** | 架构清理与工程债 | 分层剥离(§A1 目标态:config 域下沉 / domain 去 tauri / Win32 收 infra / 拆 commands)+ Schema 合并 + 错误统一 + 事件名常量化 |
| **0.20** | 能力扩展向量版 | zvec 向量基础设施 / 记忆向量召回(混合检索升级 FTS5)/ RAG 知识库 / AI 生成 Skill |

**解耦智慧**:先验证大脑(0.9 文本闭环),再加感官(0.10 语音)。0.12-0.14 是 AI 能力架构的「搭骨架 → 扩展 → 收敛重构」三步。**核心原则:零嵌入模型依赖——0.13 所有功能在用户只有 chat 模型时也完整可用,向量版留 0.20**。

> 各版完整实现见对应 `phases/`。Skill ≠ Tool:Skill 注入 preamble(教 AI 怎么做),Tool 进 tool 池(让 AI 能做什么)。

---

## §A10 ADR-001:Agent 后端策略(附录)

> **决策**:Blink 的 agent 执行端**坚持用 rig-core 自建**,不引入 opencode / pi 等现成 agent 产品作为执行后端。
>
> **状态**:✅ 已采纳(2026-07-27)。原独立 ADR,主题与 §A9 护城河强相关,并入此附录。

### §A10.1 定位分层(核心事实)

```
┌─────────────────────────────────────────┐
│  产品层:Blink(全局快捷入口 + 对话)       │  ← Blink 在这层
├─────────────────────────────────────────┤
│  Agent 产品层:opencode / pi / Cursor    │  ← 编码专用整机
├─────────────────────────────────────────┤
│  Agent 框架层:rig-core(Blink 在用)     │  ← rig 在这层
├─────────────────────────────────────────┤
│  Provider 层:ollama / OpenAI / ...     │
└─────────────────────────────────────────┘
```

**关键区分**:rig-core 是「零件」(provider 抽象 + tool calling + agent loop 原语,你拼成产品);opencode / pi 是「整机」(已完整的、面向编码场景的 agent 产品,且是**和 Blink 同层但不同场景的另一个产品**,是竞品而非依赖)。

### §A10.2 三层错位(为何 opencode 不适合当 Blink 后端)

1. **场景错配**:opencode/pi 绑定 cwd、为代码仓库设计(LSP/bash/快照)。Blink 是全局快捷入口 + 上下文感知助手,对话场景是「翻译这段」「截图识别」,不是「在仓库里编码」。重机制是死重甚至干扰。
2. **违反产品边界**:opencode 自带完整 AI 运行时(管 session/context/快照)。把它当后端 = Blink 降级成 opencode 的前端壳,0.12-0.20 在 agent 能力上的差异化投入全部作废。
3. **报废护城河**:0.13.4 MCP server(暴露 Blink 能力给生态)、0.20 RAG 是 Blink 要构建的独特价值。外包 agent 端后,Blink 自己的 Capability 反而要反向给 opencode 写 MCP server 才能被其 agent 用——绕一大圈,且与 Blink 自己的 MCP server 护城河功能重叠。

### §A10.3 坚持自建的四点理由 + 唯一缺口

**理由**:① 抽象层正确(地基与建筑的关系)② 产品边界守得住(rig 不假设上层改文件,Blink 不背「快照/LSP/bash」重包袱,符合常驻内存 < 300MB)③ 架构主权完整(Tool 适配层/Memory/preamble/gating/未来 MCP server 全在自己手里)④ 0.13/0.14/0.20 投入不白费。

**唯一缺口——context 压缩**:rig 只给原语(hook)不给策略,但 opencode/pi 这块**也是各自自建**(行业常态,非 Blink 劣势)。Blink 的路径更优:0.13 FTS5 召回(零嵌入依赖)→ 0.13 token-aware 窗口 → 0.20 向量召回(语义),「窗口 + 召回」比 opencode 的「超限粗暴截断 + LLM 摘要」更精细。

### §A10.4 现成 agent 的正确用法

ADR-001 不否定「利用现成 agent 的能力」,只是否定「把执行端交给它们」。正确用法:通过 `claude -p` / `opencode serve` 等单次任务接口,把外部 agent 包装成 **subagent(agent-as-tool)**,让 Blink 的 supervisor 在需要「复杂文件操作 / 长任务编排」时调用。保留架构主权,又借力现成生态——是 0.21+ 候选方向(详见 `../roadmap.md`)。

### §A10.5 何时重新评估此决策

触发条件(任一):① 产品定位漂移(决定内置完整编码 agent,直接对标 opencode/Cursor,应单独立项)② 需要 AI 自主操作用户文件系统(「自动整理下载文件夹」类长任务成核心场景,届时优先评估「外部 agent 当 subagent」而非整体外包)③ rig-core 维护停滞 / 重大破坏性变更(地基不可靠时重选地基)。
