# Blink — 产品总览与文档导航

> **定位**: Windows 丝滑启动器 —— 感知用户上下文、主动推荐动作，让任何操作都比原来的路径更快。
>
> **状态**: 
> 0.9 完成（Agent 地基 + Capability 能力协议层）；
> 0.10 完成（语音输入）；
> 0.11 完成（插件 AI toolchain + 截图/OCR/图上翻译）；
> 0.12.0-0.12.7 全部完成（DB 四层拆分 / Provider 模型统一 / ollama / Tool 适配层 / 独立 chat 窗口 / Alt+Q chord / 多对话管理 / SQLite 持久化 memory / Tool loop / 体验修复 / 功能增强 / 对话分组 / 布局体系化），测试基线 875 passed。0.13 扩展 MCP 双向（client + server）+ CLI 化 + token-aware context 压缩 + 记忆 FTS5 召回 + Skill 约定式（SKILL.md，零嵌入模型依赖），测试基线 1045 passed；**0.14 能力协议重构**（Capability/Action 边界钉死 + Cap 协议分层：插件只吐纯 data，投影规则上移 manifest + 四出口投影引擎收敛）；0.20 升级向量召回 + RAG（原 0.14 顺移）；0.15-0.19 留作中间优化与重构空间；0.21+ 候选含外部 agent 作 subagent（见灵感卷）。
>
> **更新时间**: 2026-07-29

---

## 一、产品核心理念

### 1.1 不只是启动器

**目标：做一个极其丝滑的启动器，并且把常用的功能都丝滑融合，使用 Chord 模式来调用各种增强能力，不止是启动器。**

| 维度 | 说明 |
|---|---|
| **唤起方式** | Alt+Space(tap)唤起,按住不松开进入 Chord 模式直接触发动作 |
| **核心体验** | 唤起→输入→执行,全程&lt;1秒,比鼠标/菜单更快 |
| **P0 至上** | 如果用户按快捷键后不能立即输入,其他所有功能都没有意义 |
| **演进方向** | 被动搜索 → Context 感知主动推荐 → AI 辅助操作 |

### 1.2 关键性能指标

| 指标 | 目标 |
|---|---|
| 快捷键唤起延迟 | &lt; 50 ms |
| 输入首个结果延迟 | &lt; 20 ms |
| 常驻内存 | &lt; 300 MB |
| 输入焦点成功率 | &gt; 99.9% |

---

## 二、文档导航

### 2.1 产品设计四卷 + 灵感卷 + ADR

> 跨版本通用的**产品决策留档**（为什么这样设计）。每卷聚焦一个层面，可独立阅读。

| 文档 | 内容 |
|---|---|
| **[product-interaction-交互.md](./product-interaction-交互.md)** | ✅ **交互体验层** —— 产品定位、唤起/焦点/IME、搜索双 lane、右键菜单、Chord 模式、i18n |
| **[product-platform-平台.md](./product-platform-平台.md)** | 🔧 **平台扩展层** —— 插件系统 + 呈现权 surface 模型、**四域架构 + ExecArg 类型墙**、意图路由、**三个统一入口 trait**、AI 能力方向（Capability/Action 边界）|
| **[product-context-future-感知.md](./product-context-future-感知.md)** | 🔮 **感知与未来** —— Awareness 环境感知（UIA 划词/剪贴板/前台）、主动建议、隐私安全 |
| **[product-principles-原则.md](./product-principles-原则.md)** | 📋 **横切原则** —— 已知取舍、日志规范、演进时间线、**最小操作路径准则、可辨识度与视觉一致性** |
| **[_inspiration-external-agent-subagent.md](./phases/_inspiration-external-agent-subagent.md)** | 💡 **灵感卷** —— 外部 agent（opencode/pi/claude-code）作 subagent 调用的可行性调研与 0.21+ 候选方向 |
| **[_adr-001-agent-backend-strategy.md](./phases/_adr-001-agent-backend-strategy.md)** | 🧭 **ADR-001** —— Agent 后端策略：为何坚持 rig-core 自建，不用 opencode/pi 当执行端 |

### 2.2 各阶段技术实现文档

> `phases/` 目录按版本沉淀**技术实现决策**——为什么这样做、踩过的坑、验收标准。

| 阶段 | 文档 | 核心内容 | 状态 |
|---|---|---|---|
| **0.1** | [phases/0.1-base.md](./phases/0.1-base.md) | P0 基础交互 + 热键/窗口/焦点/IME + 搜索 + 配置系统 | ✅ 完成 |
| **0.2** | [phases/0.2-core-plugin-design.md](./phases/0.2-core-plugin-design.md) | Service 骨架 + SearchEngine trait + 双 lane 搜索 + 图标懒加载 | ✅ 完成 |
| **0.3** | [phases/0.3-plugin-skeleton.md](./phases/0.3-plugin-skeleton.md) | 插件系统骨架(独立进程+stdio JSON)+ 热键物理态重构 | ✅ 完成 |
| **0.4** | [phases/0.4-intent-router.md](./phases/0.4-intent-router.md) | 意图引擎 RuleRouter + Context 层 + ip 插件 | ✅ 完成 |
| **0.5** | [phases/0.5-config-search-extension.md](./phases/0.5-config-search-extension.md) | 配置架构统一 KV + Everything 文件搜索 + 右键菜单 + 主题系统 | ✅ 完成 |
| **0.6** | [phases/0.6-plugin-packaging-scripting.md](./phases/0.6-plugin-packaging-scripting.md) | 插件打包路径 + Python/Node.js 脚本支持 + 统一错误处理 | ✅ 完成 |
| **0.7** | [phases/0.7-plugin-ecosystem-local-search.md](./phases/0.7-plugin-ecosystem-local-search.md) | 插件生态(翻译/剪贴板历史)+ 本地搜索 Fallback + 性能统计 | ✅ 完成 |
| **0.8** | [phases/0.8-context-interaction.md](./phases/0.8-context-interaction.md) | 感知与操作层 —— UIA 划词 + 内置动作抽象 + Autosuggestion + 翻译 Context 路由 + 四域架构 + Chord 交互 + Alt+A 截图（0.8.6 架构固化大部分已落地，两项遗留推 0.9 前） | ✅ 归档收尾 |
| **0.9** | [phases/0.9-ai-layer.md](./phases/0.9-ai-layer.md) | Agent 地基 —— rig-core 全 buildin 直编 + 统一 tool 架构 + Provider 多档 + 主窗口文本闭环 + 插件 tool-call + 供应商配置 UI + 前端架构重整 + Capability 能力协议层 | ✅ 完成 |
| **0.10** | [phases/0.10-voice-agent.md](./phases/0.10-voice-agent.md) | 语音输入 —— STT + 语音打字(G1 主窗口输入 / G2 输入法上屏)+ 伪流式 VAD 切句 + FunASR 本地化 + SendInput 文本注入 | ✅ 完成 |
| **0.11** | [phases/0.11-plugin-ai-toolchain.md](./phases/0.11-plugin-ai-toolchain.md) | 插件通信契约重设计 + AI 调用插件链路完善 + 截图标注增强 + OCR word 级链路 + 阅读模式 + 翻译衔接 + 水印独立图层（0.11.0~0.11.9） | ✅ 完成 |
| **0.11.10** | [phases/0.11-plugin-ai-toolchain.md](./phases/0.11-plugin-ai-toolchain.md#210-图上翻译--截图交互重构01110) | 图上翻译 + 截图交互重构 —— `overlayLayer` 单例图层（识别/翻译共用一层,`mode` 切换）+ 面板降级为召唤式抽屉 + 工具栏加[选取]默认工具 + 预热 OCR + 误点保护 + 命名 OCR→识别 + line 级批量翻译 + 背景遮罩三档 + 字号自适应 | ✅ 基本完成 |
| **0.12** | [phases/0.12-ai-ecosystem.md](./phases/0.12-ai-ecosystem.md) | AI 能力架构搭建（0.12.0 基础设施 / 0.12.1 对话窗口 + Alt+Q chord / 0.12.2 Chat 体验优化 / 0.12.3 多对话 + SQLite memory + Tool loop / 0.12.4 体验修复 / 0.12.5 功能增强 / 0.12.6 对话分组 + 系统提示词） | ✅ 完成 |
| **0.13** | [phases/0.13-ai-capability-expansion.md](./phases/0.13-ai-capability-expansion.md) | AI 调用能力扩展基础版 + 开放（MCP client 消费外部 tool / MCP server 暴露 Blink 能力护城河 / 自身 CLI 化 / token-aware context 压缩补 0.12.3 滑动窗口缺口 / 记忆 FTS5 召回 SQLite 全文检索零嵌入模型依赖 / Skill 约定式复用 ~/.claude/skills 等通用目录+preamble 注入） | ✅ 完成 |
| **0.14** | [phases/0.14-capability-protocol-refactor.md](./phases/0.14-capability-protocol-refactor.md) | **能力协议重构** —— Capability/Action 边界钉死（删 ActionTool，AI 永不直接调 Action）+ Cap 协议分层（插件只吐纯 data，投影规则 pointer/desc/actions 上移 manifest 做代理）+ 四出口投影引擎收敛（主窗口 AI / 对话窗口 AI / CLI / MCP 共用 canonical） | 📋 规划中 |
| **0.20** | [phases/0.20-ai-vector-moat.md](./phases/0.20-ai-vector-moat.md) | AI 调用能力扩展向量版（zvec 向量基础设施 / 记忆向量召回混合检索 / RAG 知识库 / AI 生成 Skill） | 📋 规划中 |

### 2.3 其他参考

| 文档 | 说明 |
|---|---|
| **[CLAUDE.md](../../CLAUDE.md)** | 开发规范 + 模块拆分 + Tauri Commands 清单 + 常见问题 |
| **README.md** | 项目介绍、构建运行指南(项目根目录) |

---

## 三、版本里程碑速览

| 版本 | 核心特性 | 状态 |
|---|---|---|
| 0.1 ~ 0.7 | 基础交互 → 插件生态 → 剪贴板历史 → 性能统计 | ✅ 全部完成 |
| **0.8.0** | UIA 划词文本感知 + Context 感知路由（内置动作侧）+ 内置动作抽象升级 | ✅ 完成 |
| **0.8.1** | Autosuggestion / Ghost Text 输入补全（首拼降级） | ✅ 完成 |
| **0.8.2** | 翻译插件 Context 感知路由 + `needs_translation` + `PluginSettingResolver` trait | ✅ 完成 |
| **0.8.3** | 感知交互统一：`Suggestion` 抽象 + Context 转 Ghost 采纳 + 智能感知面板 + Ghost 本地化 + origin 提示 + **awareness 域重构** | ✅ 完成 |
| **0.8.4** | **四域架构重构**：route 断 Awareness + ExecArg 类型墙 + RankingHint Surface Booster + Suggestion 覆盖非空 query | ✅ 完成 |
| **0.8.5** | **Chord 模式底层能力**：注册机制 + 独立悬浮窗 + Alt+Q 划词 + Alt+C 剪贴板（EngineTakeover）+ 配置面板 | ✅ 完成 |
| **0.8.6** | **架构固化**：Action trait / SuggestionProducer + Arbiter / ConfigStore / SearchService 拆分 / AppContext 真依赖容器 / 内置动作 i18n（为 0.9 铺物理骨架，纯横向重构） | ✅ 完成 |
| **0.8.7** | Alt+A 区域截图（DWM Cloak + BGRA 全链路 + 快速 PNG，总感知延迟 ~320ms） | ✅ 完成 |
| **0.8.8** | 收尾归档（文档同步 + 遗留项梳理，剩余优化项进 0.9 前 chore 池） | ✅ 完成 |
| **0.9.0** | 依赖前置升级（tauri 2.11.5 / reqwest 0.13）+ rig-core 引入 + 统一能力 schema + Action trait tool-call 进化 + danger_class 元数据 | ✅ 完成 |
| **0.9.1** | Provider 抽象（LLM + STT 预留，全 buildin 走 rig）+ 密钥安全（Credential Manager）+ 三档配置页 | ✅ 完成 |
| **0.9.2** | 主窗口文本闭环（rig Provider 层 + Chat Completions）+ 四筛子决策树 + tool_call 执行链路 + SLO 埋点 | ✅ 完成 |
| **0.9.3** | 插件 tool-call + ActionRegistry 改 RwLock + Tool 分组聚合 | ✅ 完成 |
| **0.9.4** | 供应商配置 UI（预设 Picker + 模型自动发现 + 连通测试） | ✅ 完成 |
| **0.9.5** | 前端架构重整（Open Props + CSS 七层 + JS 拆分）+ AI 调用体验优化 | ✅ 完成 |
| **0.9.6** | 收尾：README 中英分拆 + AI 配置/调用体验打磨 + 命令与服务微调 | ✅ 完成 |
| **0.9.7** | Capability 能力协议层（原子能力 + 统一声明/返回 + inventory 注册 + 截图/剪贴板拆解 + 接 AI tool 池） | ✅ 完成 |
| **0.10.0** | STT 接入（云端 rig 跑通）+ 语音 chord + G1 主窗口语音输入 | ✅ 完成 |
| **0.10.1** | G2 文本注入通道（Clipboard+Ctrl+V）+ 语音输入法上屏 | ✅ 完成 |
| **0.10.2** | 本地 STT（FunASR + uv 自管理）+ 统一日志 + 空间管理 | ✅ 完成 |
| **0.10.2.1** | CPU/CUDA + 自动启动 + 语音错误反馈 + 波形统一 | ✅ 完成 |
| **0.10.3** | blink_stt_server 统一服务 + SendInput Unicode + 热词/ITN | ✅ 完成 |
| **0.10.4** | 伪流式 VAD 切句定稿 + 累积预览 + 移除真流式 + 架构清理 | ✅ 完成 |
| **0.10.5** | 收尾体验优化（VAD 滑动条 + 高级选项 UI + 文档精简） | ✅ 完成 |
| **0.11.x** | 插件通信契约重设计 + AI 调用插件链路完善 + 截图标注增强 + OCR word 级 + 阅读模式 + 翻译衔接 + 水印独立图层（0.11.0~0.11.9）| ✅ 完成 |
| **0.11.10** | 图上翻译 + 截图交互重构（`overlayLayer` 单例 + 识别/翻译共用一层 + 面板召唤式抽屉 + 选取工具 + 预热 OCR + line 级批量翻译 + 命名 OCR→识别）| ✅ 完成 |
| **0.12.0** | 基础设施抽取与清账（投影统一 / DB 四层拆分 / Provider 模型统一 / ollama 接入 / Tool 适配层 + 危险确认闭环 / CapabilityRegistry 动态注册 / 存储页优化）| ✅ 完成 |
| **0.12.1** | 对话窗口骨架（rig Agent spike / AgentProvider / Alt+Q chord / ChatService / chat IPC / 流式 Markdown / 工具可视化 + 危险确认）| ✅ 完成 |
| **0.12.2** | Chat 体验优化（思考块 / 无边框 / 模型选择 / Provider 标签 / 复制按钮 / Tool 结果 / Token 用量 / Provider 变更响应 / 语音输入）| ✅ 完成 |
| **0.12.3** | 对话机制（多对话管理 + SQLite 持久化 memory + 滑动窗口 + 重启恢复 + Tool loop 触顶提示）| ✅ 完成 |
| **0.12.4** | 体验修复与优化（侧边栏 / 模型下拉 / 对话标题 / 设置跳转 / 语音输入 / 工具调用渲染 / 消息宽度）| ✅ 完成 |
| **0.12.5** | 功能增强（引导泡泡 / LLM 标题命名 / 设置页对话配置 / 消息编辑重发 / 导出 / 代码高亮）| ✅ 完成 |
| **0.12.6** | 对话分组（多层文件夹 + 分组系统提示词 + 拖拽排序 + 折叠持久化 + 内联管理）| ✅ 完成 |
| **0.13.x** | AI 调用能力扩展基础版 + 开放（MCP client / MCP server 护城河 / CLI 化 / token-aware context 压缩 / 记忆 FTS5 召回 / Skill 约定式 SKILL.md）| ✅ 完成 |
| **0.14.x** | **能力协议重构**（Capability/Action 边界钉死 / Cap 协议分层：插件只吐纯 data + manifest 投影规则 / 四出口投影引擎收敛）| 📋 规划中 |
| **0.15-0.19** | 中间优化与重构空间（候选未定案）| 🔮 候选 |
| **0.20.x** | AI 调用能力扩展向量版（zvec 向量基础设施 / 记忆向量召回 / RAG 知识库 / AI 生成 Skill）| 📋 规划中 |
| **0.21+** | 候选：外部 agent（opencode/pi/claude-code）作 subagent 调用 / 事实记忆（tool-based）/ proactivity 深化（见灵感卷）| 🔮 候选 |

---

## 四、快速开始

**想了解某方面的设计?**

| 你想了解 | 看这里 |
|---|---|
| 热键为什么是 Alt+Space? Chord 模式怎么工作? | [product-interaction-交互.md §2](./product-interaction-交互.md) |
| 插件系统怎么设计的? 意图路由怎么实现? | [product-platform-平台.md §4-5](./product-platform-平台.md) |
| 四域架构（Awareness/Suggestion/Routing/Execution）铁则? | [product-platform-平台.md §5.0](./product-platform-平台.md) |
| 三个统一入口（Capability/Action / SuggestionProducer / ConfigStore）? | [product-platform-平台.md §7](./product-platform-平台.md) + [phases/0.8 §八](./phases/0.8-context-interaction.md) |
| 选中文本/剪贴板感知怎么做?隐私如何保证? | [product-context-future-感知.md](./product-context-future-感知.md) |
| 最小操作路径准则 / 中文不斜体 / 键盘提示统一 | [product-principles-原则.md §13-14](./product-principles-原则.md) |
| 0.8 感知与操作层在做什么? | [phases/0.8-context-interaction.md](./phases/0.8-context-interaction.md) |
| 0.9 Agent 地基 + Capability 能力协议层怎么落? | [phases/0.9-ai-layer.md](./phases/0.9-ai-layer.md) |
| 0.10 语音输入(STT + 语音打字)? | [phases/0.10-voice-agent.md](./phases/0.10-voice-agent.md) |
| 0.11 插件通信契约重设计 + AI 调用插件链路完善 + 截图标注增强 + OCR word 级链路 + 阅读模式 + 翻译衔接 + 水印独立图层? | [phases/0.11-plugin-ai-toolchain.md](./phases/0.11-plugin-ai-toolchain.md) |
| 0.11.10 图上翻译 + 截图交互重构? | [phases/0.11-plugin-ai-toolchain.md §2.10](./phases/0.11-plugin-ai-toolchain.md#210-图上翻译--截图交互重构01110) |
| 0.12 AI 能力架构搭建（对话窗口 + Alt+Q chord / 对话机制 / DB 四层拆分 / Provider 模型统一管理 / ollama+lmstudio）? | [phases/0.12-ai-ecosystem.md](./phases/0.12-ai-ecosystem.md) |
| 0.13 AI 调用能力扩展基础版 + 开放（MCP client / MCP server / CLI 化 / token-aware context 压缩 / 记忆 FTS5 召回 / Skill 约定式 SKILL.md）? | [phases/0.13-ai-capability-expansion.md](./phases/0.13-ai-capability-expansion.md) |
| 0.14 能力协议重构（Capability/Action 边界钉死 / Cap 协议分层：插件只吐纯 data + manifest 投影 / 四出口投影引擎收敛）? | [phases/0.14-capability-protocol-refactor.md](./phases/0.14-capability-protocol-refactor.md) |
| 0.20 AI 调用能力扩展向量版（zvec / 记忆向量召回 / RAG / AI 生成 Skill）? | [phases/0.20-ai-vector-moat.md](./phases/0.20-ai-vector-moat.md) |
| 为何坚持 rig-core 自建 agent，不用 opencode/pi 当执行端? | [_adr-001-agent-backend-strategy.md](./phases/_adr-001-agent-backend-strategy.md) |
| 外部 agent 能否作为 subagent 调用（整理下载文件夹类场景）? | [_inspiration-external-agent-subagent.md](./phases/_inspiration-external-agent-subagent.md) |
| 开发规范、模块拆分、Tauri Commands | [CLAUDE.md](../../CLAUDE.md) |
 
