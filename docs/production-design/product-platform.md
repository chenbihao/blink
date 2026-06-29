# Blink 产品设计 · 平台扩展卷

> **性质**:本卷沉淀**扩展与智能化层**决策——插件系统(含 surface 呈现权模型)、意图引擎路由、AI 能力。是"为什么这样设计"的留档,非技术实现。
>
> 本卷原属 `product-design.md`(产品宪法),0.5 拆分为四卷;**保留原 § 节号**以维持交叉引用稳定(本卷 = 原 §4-6)。
> 配套卷:`product-interaction.md`(交互/搜索)、`product-context-future.md`(Context/隐私)、`product-principles.md`(取舍/规范/时间线)。
> 技术实现见 `phases/`;`00-overview.md` = 原 MVP.md(总纲)。
> 更新时间 2026-06-25

---

## 4. 插件系统的产品设计

### 4.1 独立进程(安全隔离)

**决策**:插件不加载 DLL,以独立进程运行,通过 stdio JSONL 通信。崩溃不影响 core。来源:MVP §8

### 4.2 插件即召回源

插件是搜索引擎的一路召回源,不是独立功能。用户输入 → 命中插件 trigger → 插件结果与普通搜索结果一起呈现。来源:0.2 §3.5

### 4.3 呈现权模型(surface ownership)——核心产品决策(0.4)

这是从 0.3「keyword 不独占」→ 0.4 正式化的关键产品模型。

#### 核心认知:触发与呈现正交

| 维度 | 回答的问题 | 谁决定 |
|---|---|---|
| **触发(match)** | query 算不算命中该插件 | keyword/regex + 是否带参 |
| **呈现(surface)** | 命中后插件如何占用 UI 返回区 | 插件 manifest 声明 + 命中强度 |

旧设计把 `exclusive` 当成路由问题(要不要查别的引擎)。真正有价值的是**呈现权**:插件命中后,它在返回区域占多大地盘、以什么**形态**呈现。而且这个区域**未来不一定是 item 列表**(`ai xxx` → 对话界面,`fs key` → 文件搜索专界面)。

#### 三态 surface

| surface | 路由影响 | UI 呈现 | 例子 |
|---|---|---|---|
| `inline` | 不独占,一路召回混排 | 普通 item,按分排序,与应用/文件并列 | 字典、单位换算 |
| `priority` | 不独占,但**置顶** | 插件 item 排最前 + 其他引擎结果保留在下方 | 翻译(译文顶部,下面仍可搜应用) |
| `takeover` | **独占**,跳过其他引擎 | **接管整个返回区**(0.4=item 列表;未来=自定义 view) | `fs key` 文件搜索、`ai xxx` 对话 |

#### 空格 = surface 升级信号(auto 模式)

默认 `surface: auto` 的行为:

| 输入形态 | 例 | auto 解出的 surface | 理由 |
|---|---|---|---|
| 部分/模糊匹配 | `f`→fs | `inline` | 可能在搜别的,作候选 |
| 精确、无参 | `fs` | `priority` | 提示"文件搜索可用"置顶,但**不屏蔽**应用搜索(`fs` 也可能是应用名) |
| keyword + 空格 + 参数 | `fs report` | `takeover` | 明确在用该插件喂参数,接管返回区(几乎不撞应用名,接管零损失) |

**关键**:这套默认规避了短词 keyword(如 `note`)误屏蔽搜索结果(Notepad)的问题,又给了带参时的全接管体验。

#### view 字段——为未来留口子

takeover 不绑定 item 列表,协议留 `view` 字段:

| view | 含义 | 阶段 |
|---|---|---|
| `list`(默认) | item 列表 | **0.4** |
| `chat` | AI 对话界面(下方展开,走流式) | P3(`ai xxx`) |
| `custom` | 插件自定义渲染 | 更远 |

0.4 只实现 `list`,但 `Route` 与前端协议预留 view 字段——P3 接 AI 对话时不改路由层。

#### manifest 声明

```jsonc
"triggers": [
  { "type": "keyword", "keyword": "fs",
    "surface": "auto",          // auto(默认)= 无参 priority / 带参 takeover
    "takeover_view": "list" }   // takeover 时的 view
]
```

- 向后兼容:旧 `exclusive: true`→`takeover`;`false`→`inline`;不写→`auto`。
- 全局总闸:`intent.surface_takeover_enabled=false` 时所有 takeover 降级 priority。

**来源**:0.4 §3.3

### 4.4 插件动作透传前端

**决策**:插件结果带 `action` 结构(Copy/Open/自定义),payload 透传到前端 `actions.js`。前端按 action.kind + payload 执行,不依赖后端硬编码行为。

这验证了插件能真正"复制结果/打开 URL/执行自定义动作",不是纯展示。来源:0.3 §一-1

### 4.5 权限模型(产品阶段判断)

**决策**:自用阶段完全不实现权限强制。manifest 的 `permissions` 字段不解析,插件直接拥有完整能力。

**理由**:权限强制(进程级隔离、用户确认)是产品化/对外分发的前置里程碑,自用阶段跳过不阻塞核心链路。来源:0.2 §3.6

---

## 5. 意图引擎的产品设计

### 5.1 三级路由模型(0.2 定稿)

```
用户输入 → RuleRouter(规则) → 命中 → 目标引擎子集
         → 未命中 → VectorRouter(zvec) → 未命中 → AIRouter(云) → 全未命中 → Generic(全引擎召回)
```

0.4 只实现 `RuleRouter`(keyword/regex),P3 才接入 VectorRouter/AIRouter。trait 先定全,避免返工。来源:0.2 §4.1

### 5.2 意图分类推迟到 P3

**决策**:0.4 的 `route()` 返回 `Route`(呈现调度:Takeover/Mixed),而非语义枚举 `Intent`(OpenApp/Calculate/Plugin/Ai/Generic)。

**理由**:`Intent` 多意图分类的真实消费者(VectorRouter 语义分类、AIRouter)在 P3 才出现。0.4 过早引入会造空抽象——路由层只需区分"怎么占用返回区",不需要知道"用户想干什么"。等 P3 有真实分类需求时,在 `Intent` 之上派生 `Route`。

来源:0.4 §3.2 / §6-5

### 5.3 keyword 匹配策略

- 精确命中 / 前缀带参(`keyword arg`)
- 中文 keyword 支持首拼输入(`tq` → "天气")
- query 和 keyword 过同一归一化(小写 + 拼音首字母双候选)

来源:0.2 §4.2, 0.3 §一

---

## 6. AI 能力(产品方向已定,实现推迟)

### 6.1 AI 路由:逐级降级,不默认全发 AI

```
规则判断 → 未命中 → 本地小模型(可选) → 不可用/置信度低 → 云模型
```

未装本地模型时,规则未命中直接降级到云模型。来源:MVP §P3

### 6.2 本地小模型 = 可选重型插件

- 本地生成模型内存占用大,不进 core
- 作为**重型插件**,按需下载、显式启用,不启用则完全忽略
- 推理栈:llama.cpp / whisper.cpp,走 GGUF/ONNX 量化
- core 只依赖 `AIProvider` trait,具体实现由插件提供

来源:MVP §13.2

### 6.3 AI 对话 = takeover view

P3 的 AI 对话不是 item 列表,而是 `view: chat` 的 takeover 区域——下方展开对话界面,走流式 JSONL。这正是 0.4 预留 view 字段的目的。来源:0.4 §3.3
