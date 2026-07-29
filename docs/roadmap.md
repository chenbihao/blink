# 前瞻路线图 · 候选方向调研

> **状态**：🔮 前瞻调研（非承诺规划）。候选落地版本：**0.21+**。
>
> **来源**：讨论「Blink 自动整理下载文件夹」类长任务场景时启发。结论：Blink 自身的 tool 体系（读为主 + 可逆写）不足以支撑「AI 自主操作文件系统的长任务」，但**不必自己造完整的编码 agent 运行时**——可把 opencode / pi agent / Claude Code 等现成 agent **作为 subagent 调用**，借力生态而不外包架构主权。
>
> **关联**：[spec-architecture §A10](./specs/spec-architecture.md)（ADR-001 为何不当后端）· [phases/0.13 §七 CLI 化](./phases/0.13-ai-capability-expansion.md)

---

## 一、问题背景

### 1.1 启发场景：「Blink 自动整理下载文件夹」

用户说：「帮我整理下载文件夹，按类型分到子文件夹，超过 30 天没动的归档到 Archive」。

这个任务的特点：
- **需要 AI 自主做文件系统写操作**（move / rename / mkdir / delete）
- **多步编排**（扫描 → 分类决策 → 批量移动 → 验证）
- **可能出错需要回滚**（移错了要能恢复）
- **需要权限边界**（不能动系统目录）

### 1.2 Blink 现状做不到

盘点 Blink 当前的文件能力（截至 0.12.7）：

| 能力 | 实现 | 性质 |
|---|---|---|
| `search_files` | `src/domain/capability/builtins/search_files.rs` | **只读**（无 fs 写操作） |
| `open_path` | `src/domain/execution/builtin.rs:401` Action | 无副作用（调系统打开） |
| `reveal_in_explorer` | `src/domain/execution/builtin.rs:444` Action | 无副作用（资源管理器定位） |

**结论**：Blink **没有任何文件系统写 capability**（move/rename/delete/mkdir 全无）。这是 [spec-architecture §A10](./specs/spec-architecture.md)（ADR-001）所述的有意为之的产品边界——Blink 的 tool 是「读为主 + 可逆写（剪贴板）」，不是「让 AI 改用户文件」。

### 1.3 两条路

要支持这类场景，有两条路：

| 路径 | 做法 | 成本 | 风险 |
|---|---|---|---|
| **A. 自己造文件操作 capability + 事务/回滚** | 新增 `move_file`/`rename_file`/`delete_file` capability + 全局快照回滚体系（对标 opencode） | **高**——快照/回滚/权限/事务化是编码 agent 的重包袱，Blink 守「不做 AI 运行时」边界 | 偏离产品定位，背 opencode 式重机制 |
| **B. 把外部 agent 当 subagent 调用** | Blink 的 supervisor agent 通过 subagent tool，调起 opencode/pi/claude-code 处理「文件长任务」，结果回流 | **中**——复用生态，Blink 只做编排 + 结果呈现 | 依赖外部 agent 安装；但架构主权完整 |

**本卷主张路径 B**。理由：符合 [ADR-001（§A10）](./specs/spec-architecture.md) 的「Blink 不做 AI 运行时」边界，借力而非重造。

---

## 二、可行性调研结论

### 2.1 rig 支持多 agent / subagent 编排吗？

**结论：支持，且无需新概念。** rig 不提供「开箱即用的 supervisor API」，但提供两个原语足以拼出 subagent 模式：

1. **agent-as-tool 模式**：把一个 `Agent` 包装成 `impl ToolDyn`，挂进另一个 supervisor agent 的 tool 池。supervisor 像调普通 tool 一样调 subagent。
2. **手动编排**：用标准 Rust 控制流（`for`/`extract`）串联多个 agent。rig 提供 `pipeline` API（`parallel!()` / `passthrough()`）辅助。

**关键事实**：Blink 0.12.0 的 **Tool 适配层**（`CapabilityTool` / `ActionTool` impl `ToolDyn`，见 `src/domain/ai/tool_adapter.rs`）**已经在用 agent-as-tool 的同款模式**——把 Capability 包装成 ToolDyn 挂进 agent tool 池。把 subagent 包装成 ToolDyn 是同一套机制，**零新概念，零新依赖**。

> 参考实现模式见 [Implementing Design Patterns for Agentic AI with Rig & Rust](https://dev.to/joshmo_dev/implementing-design-patterns-for-agentic-ai-with-rig-rust-1o71)。社区有更高层编排框架 `rigs`（[docs.rs/rigs](https://docs.rs/rigs/latest/rigs/)），但 Blink **不需要**——手动编排 + ToolDyn 包装已够。

### 2.2 外部 agent 能被 Blink 当 subagent 调用吗？

**结论：能。** 三大现成 agent 都支持「单次任务 + 结构化输出」的 headless 模式，可被 Blink 子进程化调用：

| Agent | headless 接口 | 输出格式 | 调用方式 |
|---|---|---|---|
| **Claude Code** | `claude -p "<prompt>"`（[官方文档](https://code.claude.com/docs/en/headless)） | `--output-format json`（单对象）/ `stream-json`（流式 JSONL） | 子进程 stdin/stdout |
| **opencode** | `opencode serve`（HTTP/WebSocket server）+ [JS SDK](https://opencode.ai/docs/sdk/) | HTTP JSON + SSE 流 | HTTP client（或子进程） |
| **pi agent** | CLI 单次模式 | stdout | 子进程 |

**统一抽象**：Blink 可定义 `ExternalAgentSubagent` trait，把三种调用方式收敛到同一接口：

```rust
/// 外部 agent 作为 subagent 的统一接口（规划，0.15+ 候选）。
#[async_trait]
pub trait ExternalAgentSubagent: Send + Sync {
    /// 单次任务执行：给 prompt，返回结构化结果。
    async fn run_task(&self, prompt: &str, opts: &TaskOpts) -> Result<TaskResult, SubagentError>;
}

// 具体实现：
// - ClaudeCodeSubagent: tokio::process::Command 调 `claude -p`，解析 JSON
// - OpencodeSubagent: reqwest 调 HTTP server，收 SSE
// - PiSubagent: tokio::process::Command 调 pi CLI
```

然后包装成 `impl ToolDyn`（沿用 0.12.0 适配层模式），挂进 supervisor 的 tool 池。supervisor AI 判断「这是个文件整理任务」时，自主调用 `delegate_to_claude_code` tool。

### 2.3 难度评估

| 维度 | 难度 | 说明 |
|---|---|---|
| **subagent 机制**（rig 侧） | ⭐ 低 | 复用 0.12.0 ToolDyn 适配层模式，零新概念 |
| **外部 agent 调用**（进程/HTTP） | ⭐⭐ 中 | `tokio::process` 子进程 + JSON 解析，Blink 已有插件子进程经验（`src/domain/plugin/`） |
| **结果回流 + UI 呈现** | ⭐⭐ 中 | 复用 0.12 tool result 投影（`to_rig_tool_result()`），subagent 结果当 tool result 渲染 |
| **依赖管理**（外部 agent 是否安装） | ⭐⭐⭐ 中高 | 需检测 PATH 里是否有 claude/opencode/pi，缺失则 tool 标灰；不能强依赖 |
| **权限/审批**（subagent 能改文件） | ⭐⭐⭐ 中高 | 复用 0.12 `PendingConfirms` 危险确认闭环，subagent tool 标 `Dangerous` |

**总体判断**：路径 B 难度中等，且**大部分基础设施 Blink 已有**（ToolDyn 适配层、插件子进程经验、危险确认闭环、tool result 投影）。0.15 做是合理的，**不需要大重构**。

---

## 三、与现有规划的关系

### 3.1 不冲突，且互相增强

| 现有规划 | 与 subagent 的关系 |
|---|---|
| **0.13.0 MCP client** | subagent 是另一种「外部能力来源」。MCP 是细粒度 tool，subagent 是粗粒度「整个 agent」。两者可并存于 tool 池 |
| **0.13.3 Skill 约定式** | Skill 教 AI「怎么做」，subagent 让 AI「委托谁做」。互补 |
| **0.13.4 MCP server** | Blink 作为 MCP server 暴露能力；subagent 是 Blink 作为 client 消费外部 agent。对称 |
| **0.13.5 CLI 化** | `blink delegate` 命令可直接调 subagent，复用同一后端 |

### 3.2 为什么放 0.15+ 而不是更早

1. **前置依赖**：subagent 要有用，得先有「supervisor agent 能自主判断何时委托」——这依赖 0.13.1 token-aware 窗口（supervisor 自己的 context 要健康）+ 0.13.3 Skill（教 supervisor 何时该委托）。
2. **生态成熟度**：外部 agent 的 headless 接口仍在演进（opencode serve、claude `-p` 都是近期才稳定），早做易踩 API 变更。
3. **优先级**：0.13（MCP 双向 / CLI / 记忆召回 / Skill）/ 0.14（能力协议重构）/ 0.20（向量召回 / RAG）是「所有用户受益」的基础设施；subagent 是「进阶用户借力生态」的增强，优先级靠后合理。

### 3.3 0.21 候选范围（若启动）

如果 0.21 立项，subagent 可能是其中一环，与其他候选并列：

```
0.21（候选，未定案）
  ├─ 外部 agent 作为 subagent（本卷）          P1
  ├─ 事实记忆（tool-based，ChatGPT 式 memory）  P2（0.20 砍掉项）
  ├─ proactivity 主动建议深化（../product.md §五 感知与隐私）  P2
  └─ ...（其他）
```

---

## 四、未决问题（启动前需回答）

| # | 问题 | 方向 |
|---|---|---|
| 1 | subagent 的权限模型——它调用的外部 agent 默认能改文件，Blink 怎么约束其作用域（限定目录？）| 倾向：subagent tool 标 `Dangerous`，每次调用走 `PendingConfirms`，且支持「限定工作目录」参数 |
| 2 | 多个外部 agent 并存时，supervisor 怎么选？ | 倾向：每个外部 agent 是独立 tool（`delegate_to_claude`/`delegate_to_opencode`），supervisor 自主选；或一个 `delegate` tool 带 agent 参数 |
| 3 | 外部 agent 缺失时的降级 | 倾向：tool 标灰 + 设置页提示「安装 Claude Code 可启用文件整理能力」 |
| 4 | subagent 结果的呈现——长任务可能几分钟，UI 怎么反馈 | 倾向：subagent tool 走流式（类似 0.12 tool loop），前端显示「子任务进行中」+ 阶段性回传 |
| 5 | 是否需要「Blink 自己的轻量文件操作 capability」作为 subagent 不可用时的降级 | 开放——若社区反馈「不想装 claude 也想整理文件夹」，可补 `move_file`/`rename_file` capability（Dangerous），但不做全局快照（限定单次操作可逆） |

---

## 五、最小可行验证（PoC，若推进时先做）

启动 0.15 subagent 工作前，先做一个 1-2 天的 PoC 回答：

1. **claude `-p` 能否被 Blink 子进程化稳定调用？**（最简单的 headless 接口）——写个 50 行 spike，`tokio::process::Command::new("claude").arg("-p").arg(prompt).arg("--output-format").arg("json")`，解析 JSON 结果。
2. **把 spike 包成 `impl ToolDyn` 挂进对话窗口 tool 池**——验证 agent-as-tool 模式在 Blink 现有适配层上零摩擦。
3. **supervisor 能否自主判断何时委托？**——给个「整理下载文件夹」prompt，看 supervisor 是否会调 `delegate_to_claude` tool。

PoC 通过 → 0.15 立项；PoC 发现外部 agent 接口不稳/委托判断不可靠 → 降级或推迟。

---

## 六、参考

- [Claude Code headless `-p` 模式](https://code.claude.com/docs/en/headless)
- [opencode Server 架构](https://opencode.ai/docs/server/) · [opencode SDK](https://opencode.ai/docs/sdk/)
- [pi agent (earendil-works)](https://github.com/earendil-works/pi)
- [Implementing Design Patterns for Agentic AI with Rig & Rust](https://dev.to/joshmo_dev/implementing-design-patterns-for-agentic-ai-with-rig-rust-1o71)
- [ADR-001：Agent 后端策略（并入 spec-architecture §A10）](./specs/spec-architecture.md)
