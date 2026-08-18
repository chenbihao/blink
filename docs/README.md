# Blink 文档 

> Blink 所有设计决策与架构留档。**改核心前先读本文件，再按需深入。**
>
> 📖 **入口**：[product.md](./product.md)（产品是什么、为什么）→ [specs/](./specs/)（怎么做）→ [phases/](./phases/)（各版做了什么）。

---

## 一、三层文档，各司其职

| 层 | 文件 | 回答 | 性质 |
|---|---|---|---|
| **产品决策** | [`product.md`](./product.md) | **为什么**这么设计（定位/交互/扩展/感知/原则） | 决策争议回溯处 |
| **横切规范** | [`specs/`](./specs/) | **怎么做**（HOW 的硬约束 / 铁则，跨版本通用） | 改代码前必读 |
| **版本档案** | [`phases/`](./phases/) | **这版做了什么**（架构设计 + 实现总结 + 已知问题） | 改核心前读对应版 |

**正交关系**：product 讲信念层"为什么"，specs 讲落地"怎么做的铁则"，phases 讲"某版实际做了什么"。同一主题在这三层各有侧重，不重复堆砌——通用铁则在 specs，单版细节在 phases。

---

## 二、横切规范（specs/）

| Spec | 管 | 何时读 |
|---|---|---|
| [spec-architecture.md](./specs/spec-architecture.md) | 分层 / 四域 / Capability 唯一原子执行 / Interaction·ResultAction 边界 / 协议 / 信任边界 / AI 接入 / 护城河 | 改架构、做重构、定位代码归属 |
| [spec-frontend.md](./specs/spec-frontend.md) | CSS 七层 / token / 主题 / 图标 / 视觉铁则 / 交互铁则 / 工程债 | 写或改前端 |
| [spec-backend.md](./specs/spec-backend.md) | 编码约定 / 测试 / 日志 / 错误处理 / 事件名 / invoke / 存储 / 审计日志 | 写或改后端 |
| [spec-phase.md](./specs/spec-phase.md) | Phase 文档的结构模板 / 子版本切分 / 进度状态机 / 完成后精简规则 | 新建或维护 phase |

> specs 写作约定见 [§5.2 spec 写作约定](#52-新增产品决策时)。

---

## 三、版本档案（phases/）

| 阶段 | 文档 | 核心内容 | 状态 |
|---|---|---|---|
| **0.1** | [0.1-base.md](./phases/0.1-base.md) | P0 基础交互 + 热键/窗口/焦点/IME + 搜索 + 配置 | ✅ |
| **0.2** | [0.2-core-plugin-design.md](./phases/0.2-core-plugin-design.md) | Service 骨架 + SearchEngine trait + 双 lane 搜索 + 图标懒加载 | ✅ |
| **0.3** | [0.3-plugin-skeleton.md](./phases/0.3-plugin-skeleton.md) | 插件系统骨架(独立进程+stdio JSON)+ 热键物理态重构 | ✅ |
| **0.4** | [0.4-intent-router.md](./phases/0.4-intent-router.md) | 意图引擎 RuleRouter + Context 层 | ✅ |
| **0.5** | [0.5-config-search-extension.md](./phases/0.5-config-search-extension.md) | 配置架构统一 KV + Everything 文件搜索 + 右键菜单 + 主题 | ✅ |
| **0.6** | [0.6-plugin-packaging-scripting.md](./phases/0.6-plugin-packaging-scripting.md) | 插件打包 + Python/Node.js 脚本 + 统一错误处理 | ✅ |
| **0.7** | [0.7-plugin-ecosystem-local-search.md](./phases/0.7-plugin-ecosystem-local-search.md) | 插件生态(翻译/剪贴板历史)+ 本地搜索 Fallback | ✅ |
| **0.8** | [0.8-context-interaction.md](./phases/0.8-context-interaction.md) | UIA 划词 + 内置动作抽象 + Autosuggestion + 四域架构 + Chord + Alt+A 截图 | ✅ |
| **0.9** | [0.9-ai-layer.md](./phases/0.9-ai-layer.md) | Agent 地基(rig-core + 统一 tool + Provider + 文本闭环 + Capability 协议层) | ✅ |
| **0.10** | [0.10-voice-agent.md](./phases/0.10-voice-agent.md) | 语音输入(STT + 语音打字 + 伪流式 VAD + FunASR 本地化) | ✅ |
| **0.11** | [0.11-plugin-ai-toolchain.md](./phases/0.11-plugin-ai-toolchain.md) | 插件通信契约重设计 + AI toolchain + 截图标注 + OCR word 级 + 图上翻译 | ✅ |
| **0.12** | [0.12-ai-ecosystem.md](./phases/0.12-ai-ecosystem.md) | AI 能力架构(对话窗口 + Alt+Q + DB 四层 + Provider 统一 + 多对话 + 分组) | ✅ |
| **0.13** | [0.13-ai-capability-expansion.md](./phases/0.13-ai-capability-expansion.md) | 能力扩展(MCP 双向 + CLI + token 压缩 + 记忆 FTS5 + Skill) | ✅ |
| **0.14** | [0.14-capability-protocol-refactor.md](./phases/0.14-capability-protocol-refactor.md) | 能力协议与架构收敛（删除 ActionTool + Cap 协议/投影 + 分层清理；当时保留的本地 Action 双轨由 0.21 承接） | ✅ |
| **0.15** | [0.15-screenshot-redesign.md](./phases/0.15-screenshot-redesign.md) | 截图体验重做（标注工具 + 长截图闭环采集 + 窗口吸附与像素检查） | ✅ |
| **0.16** | [0.16-clipboard-polish.md](./phases/0.16-clipboard-polish.md) | 感知交互清爽化 + chord requires_input 架构 + 剪贴板增强 + actions 数组 + 共享 Markdown + 通用编辑器基础 + 图片/pin + 杂项打磨 + 内容流转 + chord-E/chord-S + 持久化桌面便签 + 管理与恢复 | ✅ |
| **0.17** | [0.17-enhancement-polish.md](./phases/0.17-enhancement-polish.md) | 增强与打磨（存量修复 / 窗口预热 / 托盘增强 / 搜索增强 / OCR 优化 / Cherry Markdown / 主窗口 AI 统一 / 便签深化 / Live Preview / AI 权限记忆 / 剪贴板来源增强 / Capability 投影层收敛重构（projection 移出 invoke 回归展示出口 / ip + weather 插件对齐 / weather 多天预报） | ✅ |
| **0.18** | [0.18-enhancement-chord.md](./phases/0.18-enhancement-chord.md) | 增强与 Chord 体验（存量修复 / 截图 / 便签 IR / 托盘 / 命令执行 / 输入状态流转重构 / 截图多屏 DPI 收敛） | ✅ |
| **0.19** | [0.19-capability-closure.md](./phases/0.19-capability-closure.md) | 能力闭环（便签/文本真实闭环 / 行为一致性 / 图片编辑解耦 / 自然语言设置 / AI 设置与纯对话 / MCP 生命周期） | ✅ |
| **0.20** | [0.20-content-visual-workflow.md](./phases/0.20-content-visual-workflow.md) | 高频内容与视觉工作流（便签可靠性 / 剪贴板多选 / 颜色 / 图片编辑 / 截图性能与精调） | ✅ |
| **0.21** | [0.21-capability-unification-feature-catalog.md](./phases/0.21-capability-unification-feature-catalog.md) | Capability 唯一执行入口、Action 全量分流、功能目录及 AI/MCP 出口策略 | ✅ |
| **0.22** | [0.22-local-model-runtime-ppocrv6.md](./phases/0.22-local-model-runtime-ppocrv6.md) | 本地模型运行时（隔离 Python 环境 + 通用生命周期）与 PP-OCRv6 | 📋 |

> **非版本档案**：[roadmap.md](./roadmap.md)（近期优先、条件候选与远期观察；不预占版本号）；ADR-001（Agent 后端策略）已并入 [spec-architecture §A10](./specs/spec-architecture.md)。

> 新建 phase 的结构模板与生命周期规则见 [specs/spec-phase.md](./specs/spec-phase.md)。

---

## 四、版本里程碑速览

| 版本 | 核心特性 | 状态 |
|---|---|---|
| 0.1 ~ 0.7 | 基础交互 → 插件生态 → 剪贴板历史 → 性能统计 | ✅ 全部完成 |
| 0.8.x | UIA 划词 + Context 路由 + Autosuggestion + 四域架构 + Chord + 截图 | ✅ |
| 0.9.x | Agent 地基(rig + Provider + 文本闭环 + Capability 协议层) | ✅ |
| 0.10.x | 语音输入(STT + 语音打字 + FunASR + 伪流式) | ✅ |
| 0.11.x | 插件 AI toolchain + 截图标注 + OCR + 图上翻译 | ✅ |
| 0.12.x | AI 能力架构(对话窗口 + DB 四层 + 多对话 + 分组) | ✅ |
| 0.13.x | 能力扩展(MCP 双向 + CLI + 记忆召回 + Skill) | ✅ |
| 0.14.x | 能力协议重构（删除 ActionTool + Cap 协议分层 + 投影收敛）+ 架构清理；本地 Action 双轨为历史阶段 | 🚧 0.14.7 |
| 0.15.x | 截图体验重做（标注工具 + 长截图闭环采集 + 窗口吸附与像素检查） | ✅ |
| 0.16.x | 感知交互清爽化 + chord requires_input 架构 + 剪贴板增强 + actions 数组 + 共享 Markdown + 通用编辑器基础 + 图片/pin + 杂项打磨 + 内容流转 + chord-E/chord-S + 持久化桌面便签 + 管理与恢复 | ✅ |
| 0.17.x | 增强与打磨（存量修复 + 窗口预热 + 托盘 + 搜索增强 + OCR + Cherry Markdown + 主窗口AI统一 + 便签深化 + Live Preview + 权限记忆 + 剪贴板来源增强） | ✅ |
| 0.18.x | 增强与 Chord 体验（0.18.0～0.18.6 已完成；0.18.7 收敛 Alt/Chord 输入状态流转；0.18.8 截图多屏混合 DPI 坐标收敛） | 🚧 0.18.0~0.18.8 完成 |
| 0.19.x | 能力闭环（基础感知执行 + 便签/文本闭环 + 行为收敛 + 图片编辑解耦 + 自然语言设置 + AI 设置与纯对话 + MCP 生命周期） | 🚧 0.19.1~0.19.4 完成；0.19.5~0.19.11 已规划 |
| 0.20.x | 高频内容与视觉工作流（便签 / 剪贴板 / 颜色 / 图片编辑 / 截图性能与精调） | 📋 规划中 |
| 0.21.x | Capability 唯一原子执行入口、Action 全量分流删除、功能目录与 AI/MCP 授权 | ✅ 0.21.0~0.21.7 完成 |
| 0.22.x | 本地模型运行时、FunASR 生命周期迁移、PP-OCRv6 可选后端与 WinRT 回退 | 📋 规划中 |
| 未立项 | 见 roadmap：subagent / proactivity / 同步 / 远期向量与 RAG 观察 | 🔮 候选池 |

---

## 五、运作规则

### 5.1 改核心前读什么

1. 本 README（知道有什么、在哪）
2. [product.md](./product.md)（产品决策与信念层）
3. 对应 [specs/](./specs/)（落地铁则：改前端读 spec-frontend、改后端读 spec-backend、改架构读 spec-architecture）
4. 相关 [phases/](./phases/)（实现细节与已知问题）

### 5.2 新增产品决策时

判断属于哪层：
- **跨版本铁则**（怎么做）→ 对应 spec（`specs/spec-*.md`）
- **产品决策**（为什么）→ `product.md`
- **单版实现**（做了什么）→ `phases/{version}-*.md`

**spec 写作约定**（spec ≠ 教程）：

| 写什么 | 例子 |
|---|---|
| **铁则** | "必须 / 禁止"的可执行规则，配反面案例。如"中文 UI 禁用 `font-style: italic`" / "domain 不 `use tauri::`" |
| **唯一真源** | 标明某约定只在此处定义，别处只指针不复制。如"事件名常量唯一真源 = `event_names.rs`" |
| **为什么（最小）** | 铁则附 1-2 句理由即可，深度论证在 `product.md` |
| **落地指针** | 具体实现细节指向 phases 或代码路径 |

**禁止**：把"为什么"写成产品哲学长文（那是 product.md 的事）；把单版实现细节写进 spec（那是 phases 的事）；同一铁则在多处各写一份（违反单一真源）。

**留档原则**：
- 只保留**最新最终决策**，不保留版本演进注脚（"0.3 时"、"0.4 起"）
- 反面案例、踩坑记录沉淀到 `phases/`，specs/product 只留铁则/决策
- 交叉引用可以短（`见 spec-architecture §A4`），不需要重复内容

### 5.3 phase 生命周期

见 [specs/spec-phase.md](./specs/spec-phase.md)。要点：
- 新建 phase 用模板（8 个结构块），复制骨架；里程碑总览紧随开头引用块，先展示子版本状态、内容与依赖
- 完成后精简：跨版本铁则**迁出**到 specs，phase 只留单版实现细节 + 教训 + 已知问题
- 通用约定不重复写进每个 phase——那是 specs 的事

### 5.4 文档间引用

- **纯文件名 + §号**（如 `spec-architecture.md §A2`、`0.2-core-plugin-design.md §3`）
- **改文件名/移动时**，全局搜旧引用并同步更新（AGENTS.md、src 代码注释、文档间都会引用）

### 5.5 与代码的关系

- `AGENTS.md` = **给智能体的工作流路由**（改什么先看什么）
- 本目录 = **设计留档**（为什么这么设计、怎么做、做了什么）
- 代码注释里"见 spec-architecture.md §A2 / phases/0.8 §五"即指回此目录

### 5.6 变更回写约定

**任何文档变更回写需用户确认（除了单纯的状态更新）。** 文档是决策的 single source of truth，改动影响后续所有人，不能静默修改。

---

## 六、演进约定

每个大版本完成后：

1. 在 `phases/` 新增/更新 `{version}-{topic}.md` 做实现总结（按 spec-phase 模板）
2. 跨版本通用的铁则**迁出**到对应 spec，phase 改指针（避免铁则在 N 个 phase 各写一遍）
3. 产品决策沉淀进 `product.md`（只留最终决策，不留演进注脚）
4. 里程碑级变化更新本 README §四 里程碑速览
