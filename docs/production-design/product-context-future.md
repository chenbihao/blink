# Blink 产品设计 · 感知与未来卷

> 环境感知与隐私层：Context 感知、主动建议、隐私安全。
>
> 配套：`product-interaction.md`（交互/搜索）· `product-platform.md`（插件/意图/AI）· `product-principles.md`（横切原则）· `phases/` 技术实现。

---

## 7. Context 层与主动建议

### 7.1 环境感知（Awareness）

系统级"环境感知层"。让 Blink 从被动启动器升级为智能操作入口。

**感知维度**：

| 维度 | 采集方式 | 状态 |
|---|---|---|
| 前台应用（进程 + 窗口标题） | `GetForegroundWindow` + 进程名 | ✅ |
| 选中文本 | UIA + 鼠标钩子（选词瞬间抓取） | ✅ |
| 剪贴板 | Clipboard API + SQLite 持久化 + AddClipboardFormatListener | ✅ |
| **语音输入** | STT（本地 whisper-rs / sherpa-onnx 或云端） | 🔜 0.10 |
| 活跃 URL / 编辑器文件 | 浏览器扩展 / 编辑器插件 | ⬜ 远期 |
| 系统状态（时间/网络/电源） | 系统 API | ⬜ 远期 |

**设计要点**：
- 低频采集 + 按需快照：唤起时取快照，不每次按键采集
- 敏感内容不持久化（不入 SQLite），仅驻留内存
- 敏感应用（银行、密码管理器）默认关闭感知
- Awareness 数据带 `origin` 一等标签（Selection / Clipboard），下游层零推断

**四域信任边界**：Awareness 是唯一被 Suggestion 域读的数据源。Routing 域**不能**读 Awareness（详见 [platform §5.0](./product-platform.md#50-四域架构084-起产品设计基座)）。

### 7.2 选中文本感知（UIA 划词）

**问题**：复制代码/英文后还得手动输入「翻译/解释」；90% 用户选中文本后下一步就是操作它。

**方案**：全局监听选词动作，在用户**按下 Alt 之前**就把选中文本抓下来。

**关键点**：
- 全局鼠标钩子监听 `WM_LBUTTONUP`，在选词发生的黄金时机 UIA `FindAll(TextPattern)` 抓取——**必须**在焦点未失时（`show()` 之前），否则 Electron 应用退化选区
- 覆盖范围：Win32 / WPF / Office / VS Code / Chrome / Edge / Typora（80%+ 场景）
- 已知不支持：Scintilla（跨进程消息不 Marshalled）/ Java（Access Bridge 工程量大）
- **不 Ctrl+C 兜底**：避免污染剪贴板历史，用户无法回到"没被影响"的状态

**缓存与隐私**：
- 仅存内存 `OnceLock<RwLock>`，TTL 10 秒，不入 SQLite
- 成功只记长度不记内容（debug 级）；完整内容仅 trace 级记前 100 字符
- 敏感应用黑名单生效时直接跳过

**与剪贴板的配合**：
- 划词未复制 → UIA 抓取走 `AwarenessSnapshot.selection`
- 已复制 → clipboard 走剪贴板历史
- 两条链路并行，无副作用

### 7.3 主动建议（Proactive Suggestion）

**目标**：基于 Awareness 快照预测用户下一步，**直接把动作推到面前**。

**弱意图信号该 pull 不该 push**（0.8.3 起）：Context 命中不抢首屏，改走 Ghost + Tab 采纳。感知在旁路，采纳零打扰，不采纳零代价。

**已实现的建议路径**：

| Awareness 状态 | Suggestion Producer 产 Ghost |
|---|---|
| 选中/剪贴板英文（非目标语言） | `翻译 <text>` |
| 剪贴板是 URL | `打开链接` |
| 剪贴板是文件路径 | `打开路径` / `在资源管理器定位` |

**扩展方向**（后续）：
- 选中代码 → 解释 / 重构 / 查文档
- 剪贴板是报错日志 → 分析原因 / 搜解决方案
- 终端前台且输入框空 → 最近命令 / 补全

**设计要点**：
- 建议来源可配置、可学习（基于历史采纳率）
- **默认克制，避免打扰**
- 敏感 context 字段参与本地规则建议；发云需开关
- 用户可在「上下文感知」设置面板逐条 disable

---

## 8. 隐私与安全

### 8.1 默认全本地（0.8 基线）

- 选中文本 / URL 等敏感字段**绝不默认发往云端**
- 敏感内容仅驻留内存，不入 SQLite
- 0.8 的 Awareness 是纯本地感知；0.9+ 接云端 AI 后这是**性质的变化**（从「本地感知」到「数据上云」），需要 §8.2 的强告知门

### 8.2 AI / 语音上云的强告知门（0.9+ 产品铁则）

0.9 接云端 Provider、0.10 接云端 STT 后，Context（选区 / 剪贴板 / 前台）和语音录音会发往外部。这是比 0.8 本地感知**敏感一个量级**的隐私变化——语音含声纹（生物特征）。

- **第一次启用 AI / 语音必须有强告知门**：发给谁、发什么（文本 / 录音）、存不存、能关什么
- 发 AI 前 Context 字段需用户开关 + 可脱敏；语音默认本地 STT，云端 STT 需显式开启
- 录音不持久化、不入 SQLite；成功只记时长
- 敏感应用黑名单（银行、密码管理器）生效时跳过
- 详见 [phases/0.9 §4.4](./phases/0.9-ai-layer.md)（密钥安全）+ [phases/0.10 §八](./phases/0.10-voice-agent.md)（语音隐私升级）

### 8.3 代码签名

- 开发期可忽略（但需预判杀软误报）
- 分发前必须代码签名（OV/EV），否则 SmartScreen 拦截 + 杀软误报对常驻 + 全局热键应用是致命的
