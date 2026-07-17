# Blink 产品设计 · 交互体验卷

> 用户直接感知的体验层：产品定位、唤起/焦点/IME、搜索、右键菜单、Chord、i18n。
>
> 配套：`product-platform.md`（插件/意图/AI）· `product-context-future.md`（Context/隐私）· `product-principles.md`（横切原则）· `phases/` 技术实现。

---

## 1. 产品定位

### 1.1 Universal Action Layer

Blink 不是启动器（launcher），而是**统一操作层**——感知上下文、主动推荐动作，让任何操作都比原路径更快。搜索只是入口之一，**动作执行才是终点**。

0.9+ 演进为**本地 AI 的感知与执行层**——推理大脑可插拔，感知与执行是护城河（详见 [platform §6.1](./product-platform.md)）。

### 1.2 核心原则

| 原则 | 含义 |
|---|---|
| **任何操作都比原来快** | 唤起 → 输入 → 执行，全程 &lt; 1 秒，比鼠标/菜单快 |
| **最小操作路径** | 优先更快 → 再更丝滑 → 最后更智能；弱意图信号 pull 不 push；永远保留 escape hatch（见 [principles §13](./product-principles.md#13-最小操作路径横切设计准则)） |
| **可辨识度优先** | UI 是功能面不是装饰面；中文不用斜体；对比度过 AA；弱信号不撑布局（见 [principles §14](./product-principles.md#14-可辨识度与视觉一致性横切设计准则)） |
| **P0 至上** | 用户按快捷键后不能立即输入，其他一切没有意义 |
| **配置化优先** | 用户可能想改的行为做成配置项 + 合理默认；纯内部参数不暴露 |

### 1.3 技术-产品边界

后端 Rust + 前端纯静态 HTML/CSS/JS；**无 npm / 无 bundler**——前端薄到只剩渲染层，业务逻辑全在 Rust。

---

## 2. 核心交互体验

### 2.1 唤起：Alt+Space tap

默认唤起键 = **Alt+Space（tap）**。

| 关键点 | 说明 |
|---|---|
| **不吞键** | hook 回调全程 `CallNextHookEx` 放行，Alt 仍作系统修饰键 |
| **tap/hold 区分** | keydown 记时刻，keyup 时若无其他键且时长 ≤ 阈值 → tap；否则 hold 放行 |
| **提前预热** | keydown 即异步预热窗口，keyup 确认后立即 focus + 显示 |

### 2.2 焦点与失焦

| 问题 | 方案 |
|---|---|
| 某些窗口（IDEA 终端子进程）不发失焦通知 | 看门狗 150ms 轮询 `GetForegroundWindow()`，按**进程 PID** 判定 |
| 焦点真空（子进程拉起瞬态） | `fg == NULL` 跳过本轮，避免误隐藏 |
| 唤起后焦点抖动 | invoke 后 grace period（500ms）覆盖瞬态 |

### 2.3 IME

唤起时输入法必须就绪，用户直接敲中文。已验证微软拼音 / 搜狗 / 微信输入法。这是 P0 硬要求——不支持中文 IME 的 launcher 对中国用户不可用。

### 2.4 Chord 模式（0.8.5）

**定位**：Chord = **快捷键 + 独占屏幕能力** 的复合入口。不是独立动作体系；已有动作也可以有 Chord 直达方式。只有**真需要独占屏幕**的动作才用 Chord surface。

**交互**：主窗可见 + Alt hold 状态驱动。前端 `chordEligible` 门禁 = 主窗 shown + query 空 + 结果空 + `chord_enabled=true`，同时驱动触发与 Ghost overlay 提示条显示。

**Chord 三剑客**：

| 组合键 | 动作 | Surface |
|---|---|---|
| `Alt+A` | 区域截图（0.8.7） | 全屏截图覆盖 |
| `Alt+Q` | 划词翻译 | 独立悬浮球（不抢焦点，用户去原应用选文本后 confirm） |
| `Alt+C` | 剪贴板历史 | Default（填 "剪贴板 " → `ClipboardEngine` 独占返回） |

**产品价值**：跳过搜索步骤直触发；差异化竞争（Wox/Listary/Flow Launcher 都没有）；扩展性强（新高频动作可接入）。

#### 语音输入（0.10 已完成）

按住 **Alt+Space** 说话，松开自动上屏。分两种场景（详见 [phases/0.10 §二](./phases/0.10-voice-agent.md)）：

| 模式 | 触发 | 文字去向 |
|---|---|---|
| G1 主窗口语音输入 | tap → 再 hold | blink `#query` 搜索框 |
| G2 语音输入法 | 直接 hold | 外部前台应用光标处（SendInput Unicode + Clipboard 两级回退） |

支持云端（OpenAI Whisper / Groq）和本地（FunASR SenseVoice，离线免费）双引擎，伪流式 VAD 切句 + 累积预览实现边说边出字。

### 2.5 Agent 对话窗口（未来版本）

> 0.10 原计划含 Agent 对话窗口，实际落地时聚焦于语音输入（STT + 语音打字），Agent 窗口移至未来版本。

轻量路由判定「需要多轮 / 主窗口不够展示」时，**不自动展开**——产 Suggestion「Alt+1 展开对话」，用户确认才展开。自动展开会抢焦点、打断心流。

- 复用同一 WebviewWindow，切换到 chat template（0.8.4 view 字段预留 `view: chat`）
- 流式 MD 渲染（打字机 + 代码高亮 + 复制）
- 确认反馈走统一 kbd 体系：`Alt+1` 展开 / `Alt+2` 拒绝（见 [principles §14.7](./product-principles.md)）
- ESC 返回搜索；Context 可注入（选区 / 剪贴板 / 前台，可逐项关）

---

## 3. 搜索体验

### 3.1 渐进式多路搜索（sync/async 双 lane）

| lane | 引擎 | 行为 |
|---|---|---|
| **Sync**（紧 budget≈16ms） | Calc / StartMenu / Builtin / Clipboard | 同步返回首批 |
| **Async**（不阻塞） | Plugin / File | 完成后 `emit("blink://results")` 增量推送 |

**理由**：插件/网络查询可能超时，不能拖垮首批。「首个结果 &lt; 20ms」是硬指标。

### 3.2 匹配策略

- **fuzzy 子串**：nucleo 匹配原名 + 拼音首字母 + pinyin_full，取最高分
- **历史加权**：`ln(hit+1)*0.3` 频率加权（上限 0.8），常用应用排前
- **首拼降级**（0.8.1）：`fy` 等首拼命中不独占（弱信号），Priority + 灰色 Ghost 补全 + Tab 显式升级

### 3.3 计算结果

- **本地计算**（`1+1` → `2`），不经过 AI
- 整数转浮点避免截断
- 回车自动复制到剪贴板

### 3.4 右键上下文菜单

**决策**：抑制 WebView2 原生菜单（`contextmenu` `preventDefault()` + 自定义 DOM 菜单），按**区域 + 结果类型**提供差异化动作。

**分区域菜单**：

| 区域 | 菜单内容 |
|---|---|
| 输入框 | 粘贴 / 剪切 / 复制 / 全选 |
| 列表空白处 | 打开设置 / 刷新索引 / 重新探测 Everything / 退出 |
| 结果项 | 按 source 类型动态 |

**结果项按类型**：

| source | 动作 |
|---|---|
| **file** | 打开 / 打开所在文件夹 / 复制路径 / 复制文件名 / 重置记录 / 优先级置底 |
| **app** | 打开 / 打开所在文件夹 / 复制路径 / 以管理员运行 / 重置记录 / 优先级置底 |
| **calc** | 复制结果 / 复制完整表达式 |
| **plugin** | manifest 声明的 actions 透传 |

**历史干预**：右键提供「重置该项记录」（`hit_count` 归零）。"优先级置底"需持久降权数据结构，属进阶功能后置。

### 3.5 下拉项自适应屏幕高度

按唤起所在显示器的**可用垂直空间**动态计算每页容量，而不是无脑 resize 再挪窗口。

| 屏幕 | 每页条数 |
|---|---|
| 大屏 | 9（满页，对齐 Alt+1~9） |
| 中屏 | 8 / 6 |
| 小屏 | 4（下限） |

**实现要点**：
- 后端 `GetMonitorInfoW` 取工作区 → 可用条数 = `(高度 - 固定开销) / 单项高度`，clamp `[4, 9]`
- 唤起时算一次，随 `blink://shown` 下发给前端
- 前端 `PAGE_SIZE` 为运行时可变量，`clamp_to_work_area` 兜底

---

## 9. i18n 与本地化

- **UI 文案**：统一走 i18n key（后端 `LocalizableText` + 前端 i18n 库），插件 manifest 支持 zh/en 双语字段
- **Intent 触发关键词**：多语言支持（中文用户期望 `翻译`/`查IP`，英文用户期望 `translate`/`ip`），Router 反查按 UI 语言字符集偏好选 keyword
- **本地化格式**：日期/数字/计算结果按 locale 格式化
- **键盘提示**：全项目统一走 `kbd.js::renderKey/renderCombo`，不用 macOS 符号（详见 [principles §14.7](./product-principles.md#147-键盘提示样式统一强制规则)）
