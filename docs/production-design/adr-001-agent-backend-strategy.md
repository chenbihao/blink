# ADR-001：Agent 后端策略 —— rig-core 自建 vs 现成 agent（opencode / pi）

> **决策**：Blink 的 agent 执行端**坚持用 rig-core 自建**，不引入 opencode / pi 等现成 agent 产品作为执行后端。
>
> **状态**：✅ 已采纳（2026-07-27）
>
> **关联**：[0.13-ai-capability-expansion.md](phases/0.13-ai-capability-expansion.md) / [0.14-ai-vector-moat.md](phases/0.14-ai-vector-moat.md) / [inspiration-external-agent-subagent.md](inspiration-external-agent-subagent.md)

---

## 一、背景

0.13 / 0.14 的规划逐步铺开 MCP client、记忆召回、RAG、Skill、MCP server 等能力。一个自然的疑问被提出：

> 我们是不是在朝一个「完整的 agentic 运行时」演进？如果是，为何不用 opencode / pi agent / Claude Code 这类已经做得很完善的现成 agent 作为执行端，而不是基于 rig-core 自己造？

这个疑问值得一个正式的留档回答，因为它触及架构主轴。

## 二、事实调查

### 2.1 三者的定位分层（核心事实）

```
┌─────────────────────────────────────────┐
│  产品层：Blink（全局快捷入口 + 对话）       │  ← Blink 在这层
├─────────────────────────────────────────┤
│  Agent 产品层：opencode / pi / Cursor    │  ← 编码专用整机
├─────────────────────────────────────────┤
│  Agent 框架层：rig-core（Blink 在用）     │  ← rig 在这层
├─────────────────────────────────────────┤
│  Provider 层：ollama / OpenAI / ...     │
└─────────────────────────────────────────┘
```

| 维度 | **rig-core** | **opencode**（SST/Anomaly） | **pi**（earendil-works） |
|---|---|---|---|
| 本质定位 | LLM **库/framework**（building block） | **面向用户的编码 agent 产品**（TUI/IDE/desktop） | **面向用户的编码 agent harness**（终端工具） |
| 语言 | Rust | Go(TUI) + **JS/Bun(后端)** | **TypeScript/Node**（有社区 Rust port） |
| agent loop | 提供 `AgentBuilder`，**自己组装** loop | 自己实现完整 loop（基于 Vercel AI SDK + 自建 session/快照/LSP） | 自己实现（harness + extensions/skills） |
| 对外接口 | **Rust crate，`use` 进程内调用** | **HTTP/WebSocket/SSE server**（`opencode serve` + JS SDK） | Node 包 + CLI |
| 嵌入方式 | 编译期链接 | 拉起独立 Bun 进程，HTTP 通信 | 同上，Node 进程 |
| MCP client | 有（rmcp feature） | 有 | 有 |
| Skill / subagent | 无（需自造） | 有（subagent/prompt/permission 体系成熟） | 有（核心卖点） |
| 快照回滚 | 不提供（不该假设上层在改文件） | 有（针对**文件系统**，AI 改代码后能回退） | 有 |

**关键区分**：
- **rig-core 是「零件」**——provider 抽象 + tool calling + agent loop 原语，你拼成产品。
- **opencode / pi 是「整机」**——已经是一个完整的、面向编码场景的 agent 产品，且是**和 Blink 同层但不同场景的另一个产品**。

### 2.2 能力逐项对照（Blink 关心的四件事）

| 能力 | rig 提供？ | Blink 现状 | 性质判断 |
|---|---|---|---|
| **管 session**（对话隔离） | ✅ `ConversationMemory` + `conversation_id` | ✅ 0.12.3 已做 | **已具备** |
| **管 tool 执行**（loop/错误/取消） | ✅ Agent loop + `max_turns(50)` + 取消 | ✅ 0.12.2 已做 | **已具备** |
| **管 context 压缩** | ⚠️ **有原语（hook/TokenWindow）但无策略** | ❌ 只用消息条数窗口（`SLIDING_WINDOW_SIZE=20`），无 token 压缩 | **需自建策略**（→ 0.13.3） |
| **管快照回滚** | ❌ 不碰此层 | ❌ 无 | **非 Blink 必要**（见 §2.3） |

### 2.3 快照回滚为什么 Blink 不需要

opencode/pi 的「快照回滚」是**针对文件系统**的——agent 在用户代码仓库里改文件，改错了能 git-restore 回去。**预设是「agent 在你的代码仓库里动手脚」**。

Blink 的对话场景**不碰这个层**。当前 tool 全是读为主或可逆写：
- `search_apps` / `search_files` / `read_clipboard` / `capture_screen` / `ocr_image`（读，无副作用）
- `translate_text`（无副作用转换）
- `write_clipboard`（写剪贴板，可逆）

**这是有意为之的产品边界**——Blink 是「启动器 + 助手」，不是「让 AI 在用户机器上自由编程的编码 agent」。快照回滚是编码 agent 的特殊需求，不是 agent 的通用需求。

### 2.4 三层错位（为何 opencode 不适合当 Blink 后端）

1. **场景错配**：opencode/pi 绑定 cwd、为代码仓库设计（LSP/bash/快照）。Blink 是全局快捷入口 + 上下文感知助手，对话场景是「翻译这段」「上次聊的方案」「截图识别」，不是「在仓库里编码」。重机制是死重甚至干扰。

2. **违反产品边界**：0.13/0.14 文档反复写了铁则——**「Blink 不做 AI 运行时」**。opencode 自带完整 AI 运行时（管 session/context/快照）。把它当后端 = Blink 降级成 opencode 的一个前端壳，0.12-0.14 在 agent 能力上的差异化投入全部作废。

3. **报废护城河**：0.13.4 MCP server 是 Blink 的护城河（暴露 Blink 能力给生态），0.14 RAG 是 Blink 的知识护城河。这些是 Blink 要构建的独特价值。外包 agent 端后，Blink 自己的 Capability（截图/OCR/剪贴板/搜索）反而要反向给 opencode 写 MCP server 才能被其 agent 用——绕一大圈，且与 Blink 自己的 MCP server 护城河功能重叠。

## 三、决策

**坚持 rig-core 自建。** 理由：

1. **抽象层正确**：rig 在「框架层」，Blink 在「产品层」，二者是地基与建筑的关系。opencode/pi 是「同层不同场景的整机」，是**竞品**而非依赖。
2. **产品边界守得住**：rig 不假设上层在改文件，Blink 因此不必背「快照/LSP/bash」这些编码 agent 的重包袱，符合「全局快捷入口 + 助手」定位和「常驻内存 < 300MB」指标。
3. **架构主权完整**：Tool 适配层、SqliteConversationMemory、preamble 组装、gating 四筛子、未来 MCP server 护城河——全部在 Blink 自己手里，演进路径不被外部进程绑架。
4. **0.13/0.14 投入不白费**：MCP/Skill/记忆召回/RAG 都在「Blink 自己掌控 agent loop」前提下成立。

### 3.1 唯一缺口：context 压缩

「管 context 压缩」是真实缺口——rig 只给原语（hook）不给策略，opencode/pi 这块**也是各自自建**（这是行业常态，非 Blink 劣势）。Blink 已经规划好更优的路径：
- **0.13.1 FTS5 召回**（关键词，零嵌入依赖）
- **0.13.3 token-aware 窗口**（见 0.13 文档新增章节）——补「接近上限时触发压缩」的中间机制
- **0.14.1 向量召回**（语义）

Blink 的「窗口 + 召回」路径**比 opencode 的「超限粗暴截断 + LLM 摘要」更精细**，更接近 ChatGPT 的记忆体感。

### 3.2 现成 agent 的正确用法：当 subagent / MCP server，不当后端

ADR-001 不否定「利用现成 agent 的能力」，只是否定「把执行端交给它们」。正确的利用方式见 [灵感文档](inspiration-external-agent-subagent.md)：通过 **`claude -p` / `opencode serve` 等单次任务接口，把外部 agent 包装成 subagent（agent-as-tool）**，让 Blink 的 supervisor 在需要「复杂文件操作 / 长任务编排」时调用。这条路保留 Blink 架构主权，又借力现成生态——是 0.15+ 候选方向。

## 四、何时重新评估此决策

触发重新评估的条件（任一）：

1. **产品定位漂移**：如果 Blink 决定内置一个「完整的编码 agent」（直接对标 opencode/Cursor），那是新做一个产品，应单独立项评估，而非本 ADR 范畴。
2. **需要 AI 自主操作用户文件系统**：如「Blink 自动整理下载文件夹」类长任务成为核心场景，快照/回滚/权限/事务化会变成刚需。届时优先评估「外部 agent 当 subagent」（灵感文档路径），而非整体外包。
3. **rig-core 维护停滞 / 重大破坏性变更**：地基不可靠时重选地基。

## 五、参考

- [Rig 官网](https://rig.rs/) · [rig agent 概念文档](https://docs.rig.rs/docs/concepts/agent) · [0xPlaygrounds/rig](https://github.com/0xPlaygrounds/rig)
- [opencode Server 架构](https://opencode.ai/docs/server/) · [opencode SDK](https://opencode.ai/docs/sdk/) · [opencode 内部架构深度解析](https://cefboud.com/posts/coding-agents-internals-opencode-deepdive/)
- [pi (earendil-works)](https://github.com/earendil-works/pi) · [pi 官网](https://pi.dev/)
- [Claude Code headless `-p` 模式](https://code.claude.com/docs/en/headless)
- [Implementing Design Patterns for Agentic AI with Rig & Rust](https://dev.to/joshmo_dev/implementing-design-patterns-for-agentic-ai-with-rig-rust-1o71)
