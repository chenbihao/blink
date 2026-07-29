# Blink 产品决策

> **为什么（WHY）这么设计**——产品定位、交互取舍、扩展机制、感知与隐私、横切原则。
>
> 与 `specs/`（怎么做·铁则）和 `phases/`（这版做了什么）正交。本文只留产品决策与信念层论述；落地铁则（CSS 七层、日志分级、错误处理等）见对应 spec。

---

## 一、产品定位

### 1.1 不只是启动器

**目标：做一个极其丝滑的启动器，并且把常用的功能都丝滑融合，使用 Chord 模式来调用各种增强能力，不止是启动器。**

Blink 把"选中英文 → 翻译"、"复制 URL → 打开"、"截图 → OCR 提取文字"这类多步骤操作，变成一次快捷键或一个 Tab。整个体验围绕**丝滑**展开——唤起快、响应快、操作路径短。

终极目标：感知用户上下文、主动推荐动作，让任何操作都比原来的路径更快。

### 1.2 核心原则

| 原则 | 含义 | 落地铁则 |
|---|---|---|
| **P0 至上** | 用户按快捷键后不能立即输入，其他一切没有意义 | — |
| **任何操作都比原来快** | 唤起 → 输入 → 执行，全程 < 1 秒，比鼠标/菜单快 | 见 §五 最小操作路径 |
| **可辨识度优先** | UI 是功能面不是装饰面 | 见 `specs/spec-frontend.md` |
| **配置化优先** | 用户可能想改的行为做成配置项 + 合理默认；纯内部参数不暴露 | 见 `specs/spec-backend.md §一` |

### 1.3 关键性能指标

| 指标 | 目标 |
|---|---|
| 快捷键唤起延迟 | < 50 ms |
| 输入首个结果延迟 | < 20 ms |
| 常驻内存 | < 300 MB（Tauri + WebView2 基线约 80-150MB） |
| 输入焦点成功率 | > 99.9% |

### 1.4 护城河

Blink 不是「内置 AI 的启动器」，是**「本地 AI 的感知与执行层」**。推理大脑可插拔（任意 Provider / Agent），护城河是 Web AI 客户端够不着的三样：**全局感知**（Alt+Space 全屏唤起 + UIA 划词 + 剪贴板 + 前台 + 语音）/ **本地执行**（打开文件 / 运行命令 / 截图等系统权限）/ **速度**（唤起 <50ms、首个结果 <20ms）。

**产品铁则**：任何让 Blink 变成「纯对话壳子」的设计都要拒绝。AI 是消费者（调 Blink 的 tool）兼生产者（产 Suggestion），Blink 是身体。大脑可换，身体不可换。架构论证（含 Agent 后端坚持 rig-core 自建）见 `specs/spec-architecture.md §A9`。

---

## 二、核心交互体验

### 2.1 唤起：Alt+Space tap

默认唤起键 = **Alt+Space（tap）**。

| 关键点 | 说明 |
|---|---|
| **热键默认不吞键** | hook 回调全程 `CallNextHookEx` 放行，Alt 仍可作系统修饰键。**例外**：chord 独占模式下，主窗 Alt hold 时吞 chord 键 keydown（仅字母键），避免与其他软件 Alt+A 冲突；退出 chord 即恢复放行 |
| **tap/hold 区分** | keydown 记时刻，keyup 时若无其他键且时长 ≤ 阈值 → tap；否则 hold 放行 |
| **提前预热** | keydown 即异步预热窗口，keyup 确认后立即 focus + 显示 |

### 2.2 焦点与失焦

| 问题 | 方案 |
|---|---|
| 某些窗口（IDEA 终端子进程）不发失焦通知 | 看门狗 150ms 轮询 `GetForegroundWindow()`，按**进程 PID** 判定（非死比 HWND） |
| 焦点真空（子进程拉起瞬态） | `fg == NULL` 跳过本轮，避免误隐藏 |
| 唤起后焦点抖动 | invoke 后 grace period（500ms）覆盖瞬态 |

### 2.3 IME

唤起时输入法必须就绪，用户直接敲中文。已验证微软拼音 / 搜狗 / 微信输入法。**这是 P0 硬要求**——不支持中文 IME 的 launcher 对中国用户不可用。

### 2.4 Chord 模式

**定位**：Chord = **快捷键 + 独占屏幕能力** 的复合入口。不是独立动作体系；已有动作也可以有 Chord 直达方式。只有**真需要独占屏幕**的动作才用 Chord surface。

**交互**：主窗可见 + Alt hold 状态驱动。前端 `chordEligible` 门禁 = 主窗 shown + query 空 + 结果空 + `chord_enabled=true`。

**Chord 四件套**：

| 组合键 | 动作 | Surface |
|---|---|---|
| `Alt+A` | 区域截图 | 全屏截图覆盖（选区 + 标注 + OCR + 钉图 + 图上翻译） |
| `Alt+Q` | AI 对话窗口 | 独立 chat 窗口（不抢主窗焦点） |
| `Alt+C` | 剪贴板历史 | Default（填"剪贴板 " → 独占返回） |
| `Alt+Space` hold | 语音输入 | 主窗 `#query` 或前台应用光标 |

**产品价值**：跳过搜索步骤直触发；差异化竞争（Wox/Listary/Flow Launcher 都没有）；扩展性强。

### 2.5 语音输入

按住 **Alt+Space** 说话，松开自动上屏。两种场景：

| 模式 | 触发 | 文字去向 |
|---|---|---|
| G1 主窗口语音输入 | tap → 再 hold | blink `#query` 搜索框 |
| G2 语音输入法 | 直接 hold | 外部前台应用光标处（SendInput Unicode + Clipboard 两级回退） |

支持云端（OpenAI Whisper / Groq）和本地（FunASR SenseVoice，离线免费）双引擎，伪流式 VAD 切句 + 累积预览实现边说边出字。详见 `phases/0.10`。

### 2.6 Agent 对话窗口

Chord 模式下 `Alt+Q` 唤起独立 AI 对话窗口。

轻量路由判定「需要多轮 / 主窗口不够展示」时，**不自动展开**——产 Suggestion 供用户确认才展开。自动展开会抢焦点、打断心流。

- 独立 chat 窗口 + Alt+Q chord 唤起，与主窗口单轮 AI 严格隔离（`AgentProvider` vs `AIProvider`）
- 流式 Markdown + 多对话管理（分组/拖拽/折叠）+ SQLite memory + Tool loop（dangerous tool 确认）
- 详见 `phases/0.12`

### 2.7 i18n

- UI 文案统一走 i18n key（后端 `LocalizableText` + 前端 i18n 库），插件 manifest 支持 zh/en 双语字段
- Intent 触发关键词多语言支持（中文用户期望 `翻译`/`查IP`，英文用户期望 `translate`/`ip`），Router 反查按 UI 语言字符集偏好选 keyword
- 键盘提示全项目统一走 `kbd.js`（见 `specs/spec-frontend.md §四`）

---

## 三、搜索体验

### 3.1 渐进式多路搜索（sync/async 双 lane）

| lane | 引擎 | 行为 |
|---|---|---|
| **Sync**（紧 budget≈16ms） | Calc / StartMenu / Builtin / Clipboard | 同步返回首批 |
| **Async**（不阻塞） | Plugin / File | 完成后 `emit("blink://results")` 增量推送 |

**理由**：插件/网络查询可能超时，不能拖垮首批。「首个结果 < 20ms」是硬指标。

### 3.2 匹配策略

- **fuzzy 子串**：nucleo 匹配原名 + 拼音首字母 + pinyin_full，取最高分
- **历史加权**：`ln(hit+1)*0.3` 频率加权（上限 0.8），常用应用排前
- **首拼降级**：`fy` 等首拼命中不独占（弱信号），Priority + 灰色 Ghost 补全 + Tab 显式升级

### 3.3 右键上下文菜单

抑制 WebView2 原生菜单，按**区域 + 结果类型**提供差异化动作：输入框（粘贴/剪切/复制/全选）/ 列表空白（设置/刷新/退出）/ 结果项（按 source 动态：file=open+explorer+copy path / app=open+admin / calc=copy / plugin=manifest 透传）。右键提供「重置该项记录」（`hit_count` 归零）。

### 3.4 下拉项自适应屏幕高度

按唤起所在显示器的**可用垂直空间**动态算每页容量（clamp `[4, 9]`），而非无脑 resize。后端 `GetMonitorInfoW` 取工作区 → 唤起时算一次随 `blink://shown` 下发，前端 `PAGE_SIZE` 运行时可变量。

---

## 四、平台扩展机制

> 扩展机制运行在四域架构之上（四域定义见 `specs/spec-architecture.md §A2`）。本节聚焦"扩展机制怎么运作"——插件怎么呈现、意图怎么路由、Suggestion 怎么竞争。

### 4.1 插件系统

**独立进程（安全隔离）**：插件不加载 DLL，以独立进程运行，通过 stdio JSONL 通信。崩溃不影响 core。**插件即召回源**——是搜索引擎的一路召回，不是独立功能。

**动作系统三种来源**，前端契约 `Action { kind, payload }` 是三源共用投影：

| 来源 | 触发 | 结果去向 |
|---|---|---|
| **BuiltinAction** | keyword/context → results | mix 进 results，回车 `run_builtin_action` |
| **Plugin** | keyword/context → results | mix 进 results，回车走 IPC |
| **ChordAction** | Alt+字母 → 独占屏幕/emit | 直接执行副作用 |

**权限模型（阶段判断）**：自用阶段完全不实现权限强制。manifest 的 `permissions` 字段不解析，插件直接拥有完整能力。权限强制是产品化/对外分发的前置里程碑，1.0 前不做。

### 4.2 呈现权模型（surface ownership）—— 核心产品决策 ★

**核心认知：触发与呈现正交**

| 维度 | 回答的问题 | 谁决定 |
|---|---|---|
| **触发（match）** | query 算不算命中该插件 | keyword / regex + 是否带参 |
| **呈现（surface）** | 命中后如何占用 UI 返回区 | 插件 manifest 声明 + 命中强度 |

真正有价值的是**呈现权**：命中后插件在返回区占多大地盘、以什么形态呈现。这个区域**未来不一定是 item 列表**（`ai xxx` → 对话界面，`fs key` → 文件搜索专界面）。

**三态 surface**：

| surface | 路由影响 | UI 呈现 | 例子 |
|---|---|---|---|
| `inline` | 不独占，混排 | 普通 item，按分排序 | 字典、单位换算 |
| `priority` | 不独占，**置顶** | 插件 item 排最前 + 其他引擎结果保留在下方 | 翻译 |
| `takeover` | **独占**，跳过其他引擎 | **接管整个返回区**（当前=item 列表；未来=自定义 view） | `fs key` 文件搜索、`ai xxx` 对话 |

**空格 = surface 升级信号（auto 模式）**：部分/模糊匹配 → `inline`；精确无参 → `priority`；keyword+空格+参数 → `takeover`。规避了短词 keyword（如 `note`）误屏蔽搜索结果（Notepad）的问题，又给了带参时的全接管体验。

**view 字段——为未来留口子**：takeover 不绑定 item 列表，协议留 `view`（`list` 默认 / `chat` AI 对话 / `custom` 插件自定义）。0.9 接 AI 对话时不改路由层。

### 4.3 意图引擎

```
用户输入 → RuleRouter（规则） → 命中 → 直接执行（确定性快速通道）
         → 未命中 → AI 路由（轻量模型） → 高置信 + 安全动作 → 执行
                                          → 不确定/需多轮 → 回退
         → 全未命中 → Generic（全引擎召回）
```

**Route 不是 Intent（意图分类推迟）**：`route()` 返回 `Route`（呈现调度：Takeover/EngineTakeover/Mixed），而非语义枚举 `Intent`。过早引入 Intent 会造空抽象——路由层只需区分"怎么占用返回区"，不需要"用户想干什么"。等有真实分类需求时，在 Intent 之上派生 Route。

**AI 是回退决策者，不是默认路径**——确定性命中永远走快速通道（不过 AI），保护 P0。

### 4.4 Suggestion 域 —— AI 意图判定器的天然位子 ★

Suggestion 域是唯一能读 Awareness 的层，也是**AI 意图判定器的落脚点**。三源共存：

- `KeywordProducer`（首拼 fy → fanyi、部分拼音 fan hello → fanyi hello）
- `ContextProducer`（选中英文 → 翻译建议 / 剪贴板 URL → 打开链接建议）
- `AIProducer`（"帮我打开手边链接" → open_url 建议）

三源统一走 `SuggestionProducer` trait + `SuggestionArbiter` 竞争。

**关键性质**：AI 永远只产 Suggestion，**永远不产 `Route.arg`**——类型系统禁止，守死"AI 有幻觉不能直接触发副作用"的产品原则。用户 Tab 是最后一道人类审核。

### 4.5 Surface Booster —— Context 影响首屏排序的合法路径

用户打"翻译" + 剪贴板恰好英文 → 翻译插件顶到首屏第一。这是便利性，但**不能让 Routing 偷读 Awareness**。

**机制**：Suggestion 产 Ghost 时同时产 `RankingHint { boost_plugin_id }`，`SuggestionArbiter` 独立通道汇总给 SearchService，下一轮 `route()` 把 hint 作为**排序梯标**传入——只影响 surface 排序，不影响 arg / 候选集。便利性保留，域边界不破。

---

## 五、感知与隐私

### 5.1 环境感知（Awareness）

系统级"环境感知层"。让 Blink 从被动启动器升级为智能操作入口。

| 维度 | 采集方式 | 状态 |
|---|---|---|
| 前台应用（进程 + 窗口标题） | `GetForegroundWindow` + 进程名 | ✅ |
| 选中文本 | UIA + 鼠标钩子（选词瞬间抓取） | ✅ |
| 剪贴板 | Clipboard API + SQLite 持久化 + AddClipboardFormatListener | ✅ |
| 语音输入 | STT（本地 FunASR 或云端 OpenAI/Groq） | ✅ 0.10 |
| 活跃 URL / 编辑器文件 | 浏览器扩展 / 编辑器插件 | ⬜ 远期 |

**设计要点**：低频采集 + 按需快照（唤起时取快照，不每次按键采集）；敏感内容不持久化（仅驻留内存）；敏感应用（银行、密码管理器）默认关闭感知；Awareness 数据带 `origin` 一等标签（Selection / Clipboard），下游层零推断。

**四域信任边界**：Awareness 是唯一被 Suggestion 域读的数据源。Routing 域**不能**读 Awareness（见 `specs/spec-architecture.md §A2`）。

### 5.2 选中文本感知（UIA 划词）★

**问题**：复制代码/英文后还得手动输入「翻译/解释」；90% 用户选中文本后下一步就是操作它。

**方案**：全局监听选词动作，在用户**按下 Alt 之前**就把选中文本抓下来。

**关键点**：
- 全局鼠标钩子监听 `WM_LBUTTONUP`，在选词发生的黄金时机 UIA `FindAll(TextPattern)` 抓取——**必须**在焦点未失时（`show()` 之前），否则 Electron 应用退化选区
- 覆盖范围：Win32 / WPF / Office / VS Code / Chrome / Edge / Typora（80%+ 场景）；不支持 Scintilla / Java
- **不 Ctrl+C 兜底**：避免污染剪贴板历史，用户无法回到"没被影响"的状态

**缓存与隐私**：仅存内存 `OnceLock<RwLock>`，TTL 10 秒，不入 SQLite；成功只记长度不记内容（debug 级），完整内容仅 trace 级记前 100 字符；敏感应用黑名单生效时直接跳过。

### 5.3 主动建议（Proactive Suggestion）

**目标**：基于 Awareness 快照预测用户下一步，**直接把动作推到面前**。

**弱意图信号该 pull 不该 push**：Context 命中不抢首屏，改走 Ghost + Tab 采纳。感知在旁路，采纳零打扰，不采纳零代价。

0.12 落地独立对话窗口 + 0.13 落地 MCP/Skill/记忆召回后，选中或粘贴内容后直接对话即可（AI 能调能力、能查记忆），不再需要 Suggestion 层逐条预埋 Ghost。

### 5.4 隐私与安全 ★

**默认全本地（0.8 基线）**：选中文本 / URL 等敏感字段**绝不默认发往云端**；敏感内容仅驻留内存，不入 SQLite。

**AI / 语音上云的强告知门（0.9+ 产品铁则）**：

0.9 接云端 Provider、0.10 接云端 STT、0.13 接 MCP client 后，Context（选区 / 剪贴板 / 前台）、语音录音、以及**外部 MCP tool 读到的数据**都会发往外部。这是比 0.8 本地感知**敏感一个量级**的隐私变化——语音含声纹（生物特征），MCP tool 返回的数据可能比本机 Context 更敏感（如接了数据库 / 邮件 / 企业 API 后，AI 能间接读到的内容越过本机边界）。

- **第一次启用 AI / 语音必须有强告知门**：发给谁、发什么（文本 / 录音）、存不存、能关什么
- 发 AI 前 Context 字段需用户开关 + 可脱敏；语音默认本地 STT，云端 STT 需显式开启
- 录音不持久化、不入 SQLite；成功只记时长
- 敏感应用黑名单生效时跳过

**RAG 本地文档向量化的隐私含义（0.20 规划）**：embedding 默认走本地（ollama `nomic-embed-text`，文档不上云）；切云端 embedding Provider 前显式提示文档片段出本机。

**代码签名**：开发期可忽略（但需预判杀软误报）；分发前必须代码签名（OV/EV），否则 SmartScreen 拦截 + 杀软误报对常驻 + 全局热键应用是致命的。

---

## 六、产品原则（横切）

### 6.1 已知产品取舍

| # | 取舍 | 为什么妥协 |
|---|---|---|
| 1 | 热键 hook 杀软敏感 | Alt+Space 体验优于 `RegisterHotKey`；杀软问题分发期签名解决 |
| 2 | 图标提取用 COM + 自定义协议 | 不进搜索热路径；换来常驻内存不膨胀 |
| 3 | 插件进程有冷启动延迟 | 独立进程安全隔离的代价；懒启动 + 常驻复用缓解 |
| 4 | Takeover 首批为空（async 增量） | 渐进式设计的代价；带参场景插件应够快，加占位项缓解 |
| 5 | 权限模型暂不实现 | 自用阶段跳过；产品化里程碑再做 |
| 6 | Intent 语义枚举推迟 | 路由层只需呈现调度，语义分类等有真实消费者再做 |
| 7 | SelectionCache 全局静态 | 平台层能忍，改动大收益小 |

### 6.2 最小操作路径（横切信念）

> **核心信念**：Blink 的价值不在功能数量，而在**同一目标的操作路径比原来短**。任何新功能加入前，先答：**这条路径是不是变短了？** 若不是，不做。

**三条子准则**：

| 维度 | 含义 | 衡量方式 |
|---|---|---|
| **更快** | 时间维度——从产生意图到执行动作的总耗时下降 | 用毫秒对比原路径，列唤起/输入/命中/执行四段耗时 |
| **更丝滑** | 心智维度——中断/切换/等待减少，不打断专注流 | 数一次操作中的**心智切换次数**（键盘→鼠标算、切窗口算） |
| **更智能** | 感知维度——主动利用已知上下文减少用户输入 | 数实际敲键次数 vs 无感知路径的敲键次数 |

**优先级**：优先更快 → 再更丝滑 → 最后更智能。再"智能"的推荐，若首屏慢/假阳性高，反而增加中断。

**落地四问（每个新交互自问）**：
1. 原路径几步？新路径几步？步数下降至少 1 步才值得做
2. 失败时用户能回到原路径吗？**永远保留 escape hatch**（Esc 关窗、忽略 Ghost）
3. P0 主链路会退化吗？新通道跑在**旁路**，不阻塞主链路
4. 能被用户关吗？感知类/推荐类必须能在设置页一键关闭

**感知路径铁则**：弱意图信号 pull 不 push（Context 命中走 Ghost + Tab，不抢首屏）；信号即产 Suggestion 多路竞争 top-1（前端只消费单 `Suggestion` 字段）；感知类路径无副作用兜底（UIA 划词只进内存，不改剪贴板）；manifest 声明 ≠ 用户启用。

**AI 路径铁则**：护城河是感知+执行不是推理（详见 §1.4）；两条路径分离（确定性命中走快速通道不过 AI，未命中才走 AI 路由）；AI 调用必有感知反馈（不能把延迟从唤起挪到路由）；危险动作必确认（独立于交互模式）；授权粒度按交互模式分层（主窗口每次工具检查 / 对话窗口整个会话，但 Dangerous 一律确认）；Capability 是 AI 唯一入口 Action 回归纯副作用（详见 `specs/spec-architecture.md §A4`）；本地一切按需下载（安装包 < 100MB）；AI/语音上云强告知。

### 6.3 可辨识度与视觉一致性（横切信念）

> **核心信念**：Blink UI 是**功能面**，不是**装饰面**。所有视觉决策先答"这么写用户看得清楚吗"，再答"这么写好不好看"。可辨识度输了，一切美感都是负债。

**三条子准则**：辨识度（单个字符能第一眼认出）/ 对比度（前景背景差值过 WCAG AA）/ 稳定视觉重量（同类元素不同状态下块面积/位置不跳变）。

> 落地铁则（中文不斜体、主题对比度、kbd 统一、设计 token、图标用包禁 emoji）见 `specs/spec-frontend.md`。
