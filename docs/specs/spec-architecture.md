# 架构与领域规范

> **本文是 Blink 架构的 single source of truth**——分层、四域、统一入口、能力边界、协议、信任边界、AI 接入点、护城河演进,全在这一处。
>
> 配套:`../product.md`(产品决策:为什么这么设计)· `../phases/`(每版实现)。同层 spec:`spec-frontend.md` / `spec-backend.md` / `spec-phase.md`。
>
> **读法**:本文只留**最终决策**与**结构骨架**(trait 签名只放声明 + 一句注释)。实现细节、踩坑、验收在 `../phases/`;铁则的"为什么不可妥协"在 `../product.md`。
>
> **实施状态提示**：Capability 唯一原子执行入口、`CapabilityPolicy`、调用来源/运行时门禁和统一功能目录是已定的目标架构，但代码将在 [0.21](../phases/0.21-capability-unification-feature-catalog.md) 完成物理迁移。0.21 完成前，仓库仍存在本地 Action/Capability 双轨；阅读本规范时不得误判为已经落地。

---

## §A0 导读

Blink 的架构由两层正交的骨架撑起:

| 骨架 | 性质 | 把它钉成物理实体的东西 |
|---|---|---|
| **分层架构**(§A1) | 物理——模块怎么摆、依赖往哪指 | `src/{app,domain,infra,cli}` 四目录 + 依赖方向约束 |
| **四域架构**(§A2) | 逻辑——数据/控制流怎么走、信任边界在哪 | 统一入口与 Interaction 边界(§A3) + `ExecArg` 类型墙(§A7) |

之上叠加能力体系(§A4-§A6；0.14 建立协议，0.21 规划收敛为 Capability 唯一原子执行入口)、信任边界(§A7)、AI 接入约定(§A8)、护城河与演进(§A9)。

新人路径:**§A1 分层 → §A2 四域 → §A3 统一入口 → §A4 能力边界 → §A5/§A6 协议与投影 → §A7 信任边界**。读完这六节就能定位任何代码"它该在哪一层、属于哪个域、走哪个入口"。

---

## §A1 分层架构总览

Blink 源码分四层目录,依赖**只准向下**:

```
┌──────────────────────────────────────────────────────────────┐
│  cli/        ← 自身 CLI 化(mcp-server / search / run / chat)  │  最薄,可调 domain + infra
├──────────────────────────────────────────────────────────────┤
│  app/        ← 应用层:Tauri IPC 入口 + 各服务 wiring + 配置    │  依赖 domain + infra
│              · commands(Tauri command 入口,0.14 按域拆分)      │
│              · config / ai_config / stt_config(兼容门面)       │
│              · voice(语音管线编排)                            │
├──────────────────────────────────────────────────────────────┤
│  domain/     ← 业务域:四域逻辑 + 能力协议(框架无关)           │  依赖 infra,不依赖 tauri(0.14 收敛)
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

| 层 | 职责 | 0.14 收敛结果 / 遗留 |
|---|---|---|
| `cli` | `blink mcp-server/search/run/capabilities/config/chat`,clap + 最小 Tauri app 无 GUI(0.13.5) | 已达标 |
| `app` | Tauri IPC 入口 + 服务 wiring + 配置门面 | 配置类型下沉 domain/`config` 域;commands 巨石按域拆分 |
| `domain` | 四域业务逻辑 + 能力协议,**目标框架无关** | 已去 AppHandle/Emitter/managed state 直连，生产任务已改 `tokio::spawn`；0.14.7 处理构造期 `block_on` 与测试/Spike 存量 |
| `infra` | 平台抽象(`mod.rs` 接口 + `windows.rs` 实现)+ SQLite 四库 + 工具 | 已收纳 icon/shell/lock；WinRT OCR 仍待迁移 |

**关键约束**（0.14 已建立边界并完成主体迁移，遗留见 [phases/0.14 §七~§九](../phases/0.14-capability-protocol-refactor.md)）：

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

> 实现细节见 [phases/0.14 §七~§九](../phases/0.14-capability-protocol-refactor.md)（架构审查、分层清理与前端拆分）与 [phases/0.12 §基础设施](../phases/0.12-ai-ecosystem.md)（DB 四层拆分落地）。

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

## §A3 统一入口与交互边界(物理骨架)

四域是**逻辑规范**,统一入口让四域成为**物理骨架**。0.21 完成后原子执行统一进入 Capability；需要持续用户手势/会话状态的流程由各领域 Interaction 服务承载，不再保留平行的通用 Action 执行体系:

| 域 | 统一入口 | AI 如何接入 |
|---|---|---|
| **Execution / Capability** | `Capability` trait + `CapabilityResult` + 出口策略 | 本地/AI/CLI/MCP 都解析到同一原子能力；AI 经 `CapabilityTool` 适配 |
| **Interaction** | 截图、取色、编辑、录音等领域会话服务 | AI 只能调用获准的 Capability 启动/消费交互，不能伪造用户手势或直接驱动会话内部状态 |
| **Suggestion** | `SuggestionProducer` trait + `SuggestionArbiter` | `AIProducer` register 到 arbiter,三源竞争 |
| **配置** | `ConfigStore<T>` 泛型 + `blink://config-changed` 广播 | AI Provider 配置 `impl ConfigKey for AIConfig`,前端零脚手架 |

### §A3.1 Capability —— 唯一原子执行入口

凡是能以稳定 id、有限结构化参数和明确结果完成一次调用的操作，都建模为 Capability；是否打开 Blink/系统窗口、是否产生副作用，不再决定其类型。搜索、Chord、菜单、ResultAction、AI、CLI 与 MCP 可以保留各自入口协议，但最终必须解析到同一 Capability 实现。

成为 Capability **不等于自动向 AI/MCP 开放**。代码级出口策略是硬上限，用户配置只能在其子集内授权；危险/敏感确认又独立于出口授权。`open_settings`、`sticky_manager` 可以是允许 AI 的安全 Capability，`exit_blink` 可以是仅本地 Capability，`shutdown` 可以是 AI 默认关闭且每次确认的 Dangerous Capability。

0.21 完成迁移后删除 `execution::Action`、`ActionRegistry`、`ActionContext`、`ActionOutcome`、`ExecError` 及双 Registry fallback。旧 Action 必须全量分流：可确定调用者迁 Capability；持续交互状态迁领域 Interaction；仅用于入口展示/按键绑定者迁 descriptor，禁止制造空执行体的伪 Capability。

### §A3.2 Capability trait 与出口策略

```rust
pub trait Capability: Send + Sync {
    fn id(&self) -> &str;
    fn schema(&self) -> CapabilitySchema;        // 送 LLM 的 tool schema
    fn policy(&self) -> CapabilityPolicy;        // 代码级出口上限、默认授权、运行时要求
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

AI 进 tool 池的唯一适配器是 `CapabilityTool`(`impl rig ToolDyn`)。`ActionTool` 永久删除；Agent 构建时只包装“代码允许 AI 且用户已授权”的 Capability。功能目录保存稳定身份、呈现元数据、运行时要求与出口策略，不复制 schema/风险等 Capability 自有真源。

`CapabilityPolicy` 至少表达代码级允许来源（local/AI/CLI/MCP）、各出口默认授权、确认是否可记忆与运行时要求；策略属于 Capability 的静态自述，功能目录只投影，不另存一份可漂移副本。调用边界仍须在 invoke 前复核来源和授权，不能只依赖 settings UI 或 tools/list 过滤。

`InvokeContext` 必须携带调用来源与可用运行时能力。需要 GUI 的 Capability 通过受限、语义化的 surface port（如 open_settings/sticky_manager）请求 app 层执行，不取得通用 DOM、事件或任意窗口控制权；CLI/独立 MCP 等无 GUI 运行时在 list/invoke 边界依据 runtime requirement 隐藏或返回结构化 Unsupported/InvalidState，禁止扩大 `CapabilityEnv` 成无边界的 `DomainEnv`，也禁止 getter panic。

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

**前端 API**:泛型 `get_config(key) / set_config(key, value)` 命令 + `frontend/js/shared/config-keys.js` 维护 key 常量表 + `blink://config-changed` 广播(各模块按 key 订阅)。取代散落 20+ 个 `update_*` 专用命令。

> Suggestion/Config 入口的早期落地见 [phases/0.8 §八](../phases/0.8-context-interaction.md)；Capability 唯一执行入口的实施计划见 [0.21](../phases/0.21-capability-unification-feature-catalog.md)。

### §A3.5 多协议入口，单一原子执行语义（强制）

UI Command、CLI、MCP 与各类本地 surface 可以保留各自协议入口，但只要表达同一个原子操作，就必须解析并调用同一个 Capability。协议层只负责参数适配、调用来源标记、授权、错误映射与结果投影，**禁止复制业务规则形成多套实现**。纯 UI 状态读写 command 不必伪装成 Capability；它属于 Interaction 内部协议，不是独立原子能力。

生产者与消费者的资源生命周期必须解耦：截图采集、图片编辑、资源暂存属于不同会话，消费者不得隐式依赖生产者窗口、DOM 或采集 session 仍然存活。跨调用共享大资源时使用有界引用/分页协议，不传递对临时 UI 对象的隐式引用。

---

## §A4 能力体系边界(Capability / Interaction / ResultAction)

> **0.21 定稿边界（规划中）**。0.14 删除 `ActionTool`、建立 Capability 协议；0.21 进一步消除本地 Action 与 Capability 的双执行体系。

Blink 的能力分三层:

| | **Capability**(原子执行) | **Interaction**(人机流程) | **ResultAction**(结果操作) |
|---|---|---|---|
| 定位 | 稳定 id + 有限结构化参数 + 明确结果的一次调用 | 需要持续用户手势、输入、会话状态或 UI 生命周期 | 结果项上的 Copy/Preview 或对 Capability 的参数绑定 |
| 注册/落点 | `CapabilityRegistry`，唯一原子执行注册表 | 截图/取色/编辑/录音等各领域服务；不要求巨型统一 Registry | 结果协议/展示层描述，不是执行域 trait |
| AI | 仅代码允许且用户授权者经 `CapabilityTool` 进入 tool 池 | AI 不直接驱动内部状态；可调获准 Capability 启动或消费交互 | AI 不直接执行展示操作；Invoke 绑定落到 Capability |
| 例子 | search_files / open_settings / lock / shutdown / create_sticky | 区域拖选、吸管选点、图片标注、按住说话 | Copy / Preview / Invoke(open_path) |

**核心铁则**:所有原子执行只走 Capability。是否有副作用、是否打开 UI、是否允许 AI 都不是类型分界；调用契约是否闭合才是分界。`ActionTool` 不恢复，也不得在运行时把任意入口 descriptor 动态包装成 tool。

**Capability 的 UI 边界**:Capability 可以打开/切换 Blink 或系统窗口、启动一个 Interaction，但不能伪造用户手势、直接操纵 DOM，或把“已启动交互”谎报成“用户已完成交互”。区分三层:

- **允许的一次性 UI 副作用**:打开设置、便签管理、浏览器、资源管理器、pin/编辑窗口等；返回值只确认请求是否成功受理。
- **交互启动**:Capability 可启动截图、取色、编辑等 Interaction；若后续结果尚未产生，必须明确返回“已启动、等待用户”的真实状态（当前可用 `Done.summary` 表达；需要续接时再设计 session_ref 协议），不能返回“编辑/截图已完成”。
- **禁止层**:Capability 不直接操纵 DOM、不伪造点击/拖选/输入、不绕过确认卡片，也不暴露通用 `emit_event` 一类越权 dispatcher。

`open_url`/`open_path`/`reveal_in_explorer` 已有副作用(开浏览器/资源管理器)是先例,0.19 的便签/pin cap(`create_sticky`/`pin_image`)同属此类。`CapabilityResult::Done` 变体("已写入/已打开/已锁定")即为副作用语义准备。

**危险判定的两维**(`DangerClass` + `CapabilitySchema.sensitive`):

| 维度 | 语义 | 例子 | 进 tool 池 |
|---|---|---|---|
| `DangerClass::Dangerous` | 有副作用的危险动作(改系统状态) | lock / shutdown / 写文件 | ✅ 但必确认 |
| `CapabilitySchema.sensitive` | 读隐私/敏感数据(无副作用但数据敏感) | `search_apps`(读应用列表) / `search_clipboard_history`(读剪贴板) | ✅ 0.13 MCP server 暴露时需授权 |

`CapabilityTool::is_dangerous()` 判定式 = `danger_class == Dangerous || schema.sensitive`——两者都触发人机确认流程,但语义不同(前者防"做了不该做的",后者防"泄露不该泄露的")。`sensitive` 字段在 0.11 引入、0.13 修复为活字段(此前是死字段,`is_dangerous` 未读取它)。

**出口授权与类型正交**——只有 Capability 能进入 tool 池，但并非所有 Capability 都能进入。代码级 `allowed_origins`/运行时要求是硬上限，用户配置是其子集；`Dangerous`/`sensitive` 的确认与审计再独立叠加。`exit_blink` 可是 Capability 且代码级禁止 AI/MCP；`open_settings` 可安全允许 AI；`shutdown` 可允许 AI 但默认关闭并强制逐次确认。

**Interaction 不等于旧 Action 改名**:它承载的是有生命周期的人机流程，不是另一套通用原子执行接口。开始交互、提交结果、取消会话可分别调用领域服务或 Capability，但持续状态留在所属领域。Chord/Search/Menu 只是入口或 binding，不拥有业务执行语义。

> 0.14 的历史边界与迁移背景见 [phases/0.14](../phases/0.14-capability-protocol-refactor.md)；统一执行终态与全量分流见 [0.21](../phases/0.21-capability-unification-feature-catalog.md)。

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

- **JSONPath 取值**:用 `jsonpath-rust` crate(纯 Rust,~30KB)。单值/数组通配/嵌套对象/过滤都覆盖,后续结构化数据投影可复用。不用自造 dialect。
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

`ItemAction` 是 0.14 wire 名称，语义上属于 ResultAction，不是计划删除的 `execution::Action`。0.21 后领域副作用型结果操作应优先投影为 `Invoke { capability_id, args }`；Copy/Preview 等纯展示短路径可保留专用变体。兼容期可保留旧枚举名/旧 wire shape，但执行必须落到 Capability。

**对比旧 `ItemResult` 的关键变化**:删 `title`/`subtitle`/`score`(主窗口展示概念,不该在协议层);`data` 取代 `payload`;主标题(旧 title)由前端从 `data` 派生(投影规则见 §A6)。

### §A5.3 多 action 交互约定

manifest 里 `actions` 数组**顺序即优先级**:

| 操作 | 行为 |
|---|---|
| 回车 | 执行 `actions[0]`(首个) |
| 右键 | 展开全部 actions 供选择 |

插件作者把最常用动作放第一个。空 `actions` + 纯标量 data 时,主窗口隐式提供 copy(**展示层 fallback,非协议层派生**)。

> 完整协议结构、迁移策略(一次性切换,0.x 插件全自研无兼容层)、选型理由见 [phases/0.14 §三/§四/§十一](../phases/0.14-capability-protocol-refactor.md)。

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
/// invoke 链路只做形态规范化,不挑字段——data 保留完整原始值(0.17.10 收敛)
pub fn normalize(raw: &PluginRawResult) -> CapabilityResult { ... }

/// Capability trait 独立方法,只在展示出口调用(0.17.10 从 invoke 移出)
pub trait Capability {
    // ... invoke 用 normalize,不投影 ...
    fn projection(&self) -> Option<&ProjectionRule> { None }  // default None
}
```

> **0.17.10 收敛**:旧 `project()` 同时承担"给 AI 挑数据"和"给展示挑字段"两个冲突职责——AI 拿到被 pointer 砍窄的裸字段(ip 丢地理位置、weather 丢天气状况)。收敛后 invoke 走 `normalize()`(只规范化形态,raw JSON → CapabilityResult,**不挑字段**),投影 `projection()` 独立为 Capability trait 方法(default None),只有展示出口 `to_display_text` 才按 manifest 投影。AI 出口天然拿完整 data,零 manifest 改动即修复。详见 [phases/0.17 §3.15](../phases/0.17-enhancement-polish.md)。

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

### §A7.3 AI 修改设置的信任边界（强制）

- AI 可读/可写设置必须经过**显式、类型化白名单**，禁止暴露原始 KV、任意配置路径、密钥或其他敏感字段
- 写入前向用户展示字段语义及旧值/新值；每次写入独立确认，不记忆或继承上一次写权限
- 写入时重新校验白名单、参数类型与乐观并发版本；确认只授权已展示的那一次变更，参数变化后必须重新确认
- 协议返回可审计的结构化结果，但日志和审计摘要不得记录密钥或敏感原文

---

## §A8 AI 接入点总览（0.21 目标态）

AI 通过下列受控入口接入:

| AI 产出 | 接入点 | 路径 |
|---|---|---|
| **tool-call** | Capability | AI → `CapabilityTool` → `Capability::invoke()` → `CapabilityResult` → 投影(§A6)给四出口 |
| **建议(Suggestion)** | Suggestion | `AIProducer` register 到 `SuggestionArbiter`,与 Keyword/Context 三源竞争 |
| **配置** | ConfigStore | Provider 配置 `impl ConfigKey for AIConfig` |
| tool-call(副作用或 UI 启动) | Capability | 仍经 `CapabilityTool`，并受出口授权、危险确认和运行时要求约束 |

**四域信任边界依旧成立**:AI 只能产 Suggestion / tool-call 候选,不能构造 `ExecArg::UserExplicit`。用户 Tab 采纳后才穿过 Suggestion → Execution 边界。危险 Capability AI 调用时需用户确认(§A7.2)。

**两条路径铁则**:
- **确定性路径**(< 1s):命中规则 → 直接执行,**不过 AI**,保护 P0
- **模糊意图路径**:未命中才走 AI 路由(AI 永远是**回退决策者**,不是默认路径)
- **AI 反馈铁则**:AI 调用期间主窗口必有 loading/过渡,不能让用户死等

**AI 对话 = takeover view**(0.12 已完成):AI 对话不是 item 列表,而是 `view: chat` 的 takeover 区域——独立对话窗口(Alt+Q),走流式 JSONL。这是 surface 模型 `view` 字段预留的目的。

### §A8.1 主窗口 vs 对话窗口:调度统一,记忆隔离(0.17.6 后)

> **0.17.6 收敛**:主窗口 AI 从 `SearchService::trigger_ai`(外部调度 + 单槽 `PendingAiConfirmation`)切换到 `ChatService::prompt`,与对话窗口**统一调度模型**。旧的两套独立调度(`SearchService` vs `ChatService`)已删除——主窗口不再有独立的 AI 调度路径。

**调度统一,但记忆 kind 严格隔离**:

| 窗口 | Memory kind | 持久化 | FTS5 召回 | 语义 |
|---|---|---|---|---|
| **主窗口**(AiMode) | `EphemeralConversationMemory` | ❌ 进程内 `HashMap` | ❌ | 临时性是其交互契约——ESC 即清空,Alt+Q 可提升为持久对话 |
| **对话窗口**(Alt+Q) | `SqliteConversationMemory` | ✅ SQLite | ✅(0.13.2) | 持久化对话,重启恢复 |

**铁则**:**不可混用 memory kind**——主窗口 AI 的临时性是其交互契约(ESC 即清空、不污染历史),给它接持久化 memory 会破坏语义。`ChatService` 按 `ConversationKind`(`Persistent`/`Ephemeral`)选 memory,Agent 缓存 key 追加 kind。事件通道 `CHAT_STREAM` / `CHAT_CONFIRM_ACTION` 统一,按 `target_window` 定向 emit(旧 `AI_STREAM`/`AI_CONFIRM_ACTION` 已删)。

**watchdog AI 标志**:主窗口 AiMode 下 `MAIN_WINDOW_AI_ACTIVE: AtomicBool` 置位,看门狗跳过失焦隐藏(用户正在和主窗口 AI 对话,不能因切走而关窗)。

### §A8.2 Agent 缓存：行为维度完整覆盖

`AgentProvider` 的 cache key 必须覆盖所有会改变运行时行为的维度，至少包括 provider/model、凭据 fingerprint、conversation kind/mode、Skill/tool 集合以及 preamble hash；也可在相关配置变化时显式失效。**行为维度变化后不得复用旧 Agent**。

其中 preamble 使用内容 hash 自动失效：preamble 字符串变化 → `hash_preamble()` 变化 → cache miss → 重建 Agent。Skill 激活、分组系统提示词、权限策略等如果进入 preamble，直接由内容 hash 覆盖；不进入 preamble 的行为维度必须进入 cache key 或显式失效，禁止依赖调用方“记得手工重建”。

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

**架构铁则**:**Agent 后端坚持 rig-core 自建,不用 opencode/pi 当执行端**——它们是和 Blink 同层的 agent 产品(非依赖),外包执行端会违反「不做 AI 运行时」边界,并架空 Blink 已有的 Tool/Memory/preamble/gating 能力。现成 agent 的正确用法是当受控 subagent/MCP server,不当后端。详见 [§A10](#§a10-adr-001agent-后端策略附录)。

### §A9.2 Provider 多档(能力供应商)

Provider 不只是聊天 API,是**能力供应商**:当前覆盖 LLM(`chat`/`chat_stream`)与 STT(`transcribe`),并允许未来扩展其他模型能力。一个 Provider 可只供一项(本地 whisper 只供 STT),也可供应多项。embedding / RAG 当前不进入短期路线,见 [roadmap §五](../roadmap.md#五远期观察原-020-向量--rag-规划归档)。

三档配置:**超轻模型**(标题命名、分类、翻译等低成本短任务)/ **轻量模型**(主窗口与日常对话)/ **主模型**(复杂推理与 Agent loop)。档位空缺按“超轻 → 轻量 → 主”自动降级；对话标题默认使用超轻档。搜索中的 AI Ghost 只负责建议用户进入主窗口 AI，不调用超轻档做独立意图分类。

> **不照搬全自动执行**:危险动作必确认,四域信任边界不让让步。

### §A9.3 能力体系演进时间线

| 版本 | 主题 | 架构动作 |
|---|---|---|
| **0.9.7** | Capability 能力协议层诞生 | 原子能力 + 统一声明/返回 + inventory 注册 + 截图/剪贴板拆解 + 接 AI tool 池 |
| **0.12** | AI 能力架构搭骨架 | 对话窗口 / DB 四层拆分 / Provider 模型统一 / Tool 适配层 / CapabilityRegistry 动态注册 |
| **0.13** | 能力扩展(基础版 + 开放) | MCP client/server / CLI 化 / token-aware 压缩 / 记忆 FTS5 召回 / Skill 约定式 / 0.13.7 收敛(P3 投影剔 score + 插件 Action→Capability 迁移,删 `ActionOutcome::Items`) |
| **0.14** | 能力协议与架构收敛 | 删除 `ActionTool`、AI 仅经 Capability + Cap 协议分层(§A5) + 四出口投影引擎(§A6) + 分层与工程债清理；当时保留本地 Action 双轨，后由 0.21 承接 |
| **0.19** | 能力闭环 | 窗口/图片感知 + 便签/pin 执行 + 用户/AI 双入口收敛 + ImageStash 图片跨 Capability 引用 |
| **0.21（规划）** | 唯一原子执行入口 | 旧 Action 全量分流为 Capability / Interaction / descriptor，删除 ActionRegistry 与双 Registry fallback；功能目录统一呈现及本地/AI/MCP 出口策略 |

**解耦智慧**:先验证大脑(0.9 文本闭环),再加感官(0.10 语音)。0.12-0.14 是 AI 能力架构的「搭骨架 → 扩展 → 收敛重构」三步,0.19 再补感知与执行闭环。**核心原则:零嵌入模型依赖——FTS5 + token-aware 窗口是当前正式方案,向量化与 RAG 仅保留为远期观察,不预占版本**。

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
2. **违反产品边界**:opencode 自带完整 AI 运行时(管 session/context/快照)。把它当后端 = Blink 降级成 opencode 的前端壳,0.12-0.14 已建立的 Tool/Memory/preamble/gating 主权被架空。
3. **架空统一能力底座**:Blink 已通过 Capability/MCP server 暴露自身能力。外包 agent 端后,Blink 反而要把自己的能力再包装给外部后端才能使用,形成多余反向依赖,并削弱感知与执行闭环这一真正护城河。

### §A10.3 坚持自建的四点理由 + 唯一缺口

**理由**:① 抽象层正确(地基与建筑的关系)② 产品边界守得住(rig 不假设上层改文件,Blink 不背「快照/LSP/bash」重包袱,符合常驻内存 < 300MB)③ 架构主权完整(Tool 适配层/Memory/preamble/gating/MCP server 全在自己手里)④ 感知与执行能力可以独立演进,不被外部 agent 产品绑架。

**唯一缺口——context 压缩**:rig 只给原语(hook)不给策略,但 opencode/pi 这块**也是各自自建**(行业常态,非 Blink 劣势)。Blink 当前用 0.13 token-aware 窗口 + FTS5 召回解决,保持零嵌入依赖；只有真实数据证明关键词召回不足时,才按 roadmap 门槛重新评估语义召回。

### §A10.4 现成 agent 的正确用法

ADR-001 不否定「利用现成 agent 的能力」,只是否定「把执行端交给它们」。正确用法:通过稳定的单次任务接口,把外部 agent 包装成 **subagent(agent-as-tool)**,让 Blink 的 supervisor 在需要「复杂文件操作 / 长任务编排」时受控调用。保留架构主权,又借力现成生态——这是未立项的条件候选(详见 `../roadmap.md`)。

### §A10.5 何时重新评估此决策

触发条件(任一):① 产品定位漂移(决定内置完整编码 agent,直接对标 opencode/Cursor,应单独立项)② 需要 AI 自主操作用户文件系统(「自动整理下载文件夹」类长任务成核心场景,届时优先评估「外部 agent 当 subagent」而非整体外包)③ rig-core 维护停滞 / 重大破坏性变更(地基不可靠时重选地基)。
