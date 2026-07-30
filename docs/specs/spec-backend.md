# 后端实现规范

> **怎么做（HOW）**——Rust 后端代码的硬约束 / 铁则。写或改后端代码前先读本文。
>
> 架构层铁则（分层依赖方向、四域边界、Capability/Action）见 `./spec-architecture.md`；本卷聚焦编码、测试、日志、错误、事件、存储这些横切工程规范。

---

## 一、编码约定

| 规则 | 说明 |
|---|---|
| **配置化优先** | 可选行为（默认值用户可能想改的）做成配置项 + 合理默认；纯内部参数不暴露 |
| **平台抽象预留** | 平台相关逻辑走 `infra/platform/` 的 `mod.rs` 接口 + `windows.rs` 实现，domain 不直接 `use windows::` |
| **不过度工程** | 0.x 阶段不对外发布，产品化基础设施（manifest 升级/权限强制/插件市场）1.0 前不做 |
| **架构要有前瞻性** | 精心设计持续演进，不过早腐败，不随便堆砌坏味道与技术债，持续收敛 Clean Architecture |

> 分层依赖方向的硬约束（domain 不 use tauri / infra 不反向依赖 app）见 `spec-architecture.md §A1`。

---

## 二、测试策略（务实 TDD）

- ✅ **纯逻辑/算法必须有单测**：计算、fuzzy/拼音、PNG 编码、状态机等可纯函数化的逻辑。主动把可测逻辑从平台调用里抽出来
- ❌ **Win32/GUI/Shell/Tauri 集成层免自动化**：这类调用难以稳定 mock，靠 `cargo run` 手动验证主链路
- ⚠️ **依赖系统资源的测试要可跳过**：用 `Path::exists` 守卫，缺失则跳过（不依赖 CI 桌面环境）
- ✅ **验证产物正确性**：例如断言 PNG 魔数，而不只是 `!is_empty()`

```bash
cargo test --bin blink   # 跑单测（bin crate，无 lib target）
```

---

## 三、日志规范（强制）

> **真源**：本节是日志分级的唯一权威定义。AGENTS.md 与历史文档关于日志的描述均以本节为准。

### 3.1 分级

| 级别 | 数值 | 含义 | 开启场景 | 典型用途 |
|------|------|------|----------|----------|
| **error** | 1 | 致命异常，功能受影响 | 默认全开 | 数据库失败 / 启动失败 / 关键操作异常 |
| **warn** | 2 | 潜在问题，不影响运行 | 默认全开 | 多次重试 / 配置异常但默认值兼容 |
| **info** | 3 | 关键状态变化，里程碑 | 默认全开 | 应用启动 / 配置更新 / 插件加载 / 用户关键操作 |
| **debug** | 4 | 主流程节点 | 开发/测试 | 收到搜索请求 / 返回结果数 / 分支决策 |
| **trace** | 5 | 最详细诊断 | 排查特定问题时 | HTTP 响应体 / 完整参数 / 循环内变量 |

### 3.2 铁则

1. **统一 tracing 日志**——禁止散落 `println!/eprintln!`；全部走 `tracing` 宏
2. **结构化优先**——`tracing::debug!(%query, port, "搜索 Everything")`，禁字符串拼接
3. **错误必带上下文**——`tracing::error!(%path, %e, "启动应用失败")`，禁"失败了"
4. **预期降级不告警**——Everything 没装 / 剪贴板被锁定 / 图标提取失败，用 debug/trace，不用 warn/error
5. **敏感信息永不记日志**——密码 / 剪贴板内容（除 trace 级谨慎使用）/ 用户隐私

> 关键节点要打日志，量适中等级合适；开发流程可打临时日志排查，收尾时清理。

---

## 四、错误处理规范

> 0.14 §8.2 确立并开始落地。新代码按本节写，剩余存量随自然演进收敛。

### 4.1 thiserror 强制（目标态）

- **启用 `thiserror`**（依赖已在树），用 `#[derive(thiserror::Error)]` 消除手写 `Display` + 空 `impl Error` 样板
- **补关键跨类型 `From`**（CapabilityError ↔ ExecError 等），消除散落的 `.map_err(|e| e.to_string())` 拍平
- **command 层错误走 serde 序列化**——保留结构化字段（如 `CapabilityError.kind`），前端拿到可分类展示，而非拍平的中文字符串

### 4.2 CapabilityError 透传模型

`CapabilityError { kind, detail }` 是可序列化错误模型。IPC 边界必须保留 `kind` 字段——前端据此分类展示（重试 / 配置缺失 / 权限 / 未知），而非拿到一坨中文字符串。

### 4.3 插件错误四层兜底

插件查询链路的统一错误处理（phases/0.6）：

| 错误类型 | 用户看到的反馈 |
|---|---|
| 超时 | `查询超时，请稍后重试` |
| 解释器未找到 | `未找到解释器：python，请在设置页配置` |
| 进程启动失败 | `进程启动失败：{具体原因}` |
| 进程意外退出 | `插件进程意外退出` |
| 插件主动报错 | `{插件返回的 message}` |

**设计原则**：
1. `PluginHandle::query()` 永远返回 `Ok(Vec<PluginItem>)`
2. 错误项 = 负分 PluginItem，走正常「占位符替换 → 排序 → 展示」链路
3. 四层处理：脚本端强制 UTF-8 → `PluginProcess::query()` 内部转化 → `PluginHandle::query()` 兜底 → 前端负分透传

---

## 五、事件名常量化（强制）

> 0.14 §8.2 确立并落地。

**铁则**：`blink://*` 事件名**禁止前后端各自硬编码字面量**。

- **后端真源**：集中事件名常量清单（Rust 常量），emit 处引用常量而非拼字符串
- **前端真源**：前端事件名常量模块，`TAU.event.listen()` 引用常量
- **同步**：可考虑 codegen 从 Rust 常量生成前端常量，消除手动同步
- **校验**：拼错事件名目前无编译期保护，常量化后由"引用不存在的常量"触发编译错误

当前 Rust 与前端均已建立集中事件名清单；新增或修改事件时必须同步两端，并通过字面量核对防止漂移。

---

## 六、invoke 路径收敛（强制）

> 0.14 §8.2 确立并落地。

**铁则**：前端 invoke **单一 import 来源** = `frontend/js/tauri.js`。

- **禁止**绕过桥接直接戳 `window.__TAURI__`
- **禁止**各文件重复 `window.__TAURI__?.core?.invoke ?? ...` 兼容逻辑
- 业务模块不得恢复历史上的 `__TAURI__` 绕过；需要新增原生 API 时先扩展 `tauri.js` 桥接

---

## 七、数据存储

> SQLite `%APPDATA%\blink\`（0.12.0 起四库独立写锁，`DbPools` struct 持有四个独立 `SqlitePool`，互不阻塞）。

| 库 | 内容 |
|---|---|
| **`blink_config.db`** | 配置 KV。`config(key, value, updated_at)`——`AppConfig` 6 分片门面 + `AIConfig` 第7 + `SttConfig` 第8 + 各引擎/插件配置。**未来跨机同步只同步此库** |
| **`blink_history.db`** | `history(lnk_path, hit_count, last_used_at)` 启动历史 + `clipboard_history(id, text, kind, hit_count, last_used_at)` 剪贴板历史 |
| **`blink_ai.db`** | `ai_tool_audit` AI 工具审计（0.12.0 加 `cleanup_old` 30 天 + 行数上限 10000）+ `conversations` / `messages`（对话记忆持久化） |
| **`blink_cache.db`** | `performance_metrics` 性能统计（高频写）+ `icon_cache` 图标缓存（BLOB） |

**关键业务约束**：`lnk_path` 是 history 主键——扫描产生的路径字符串**不可随意归一化/改写**，否则历史权重 key 失配。

---

## 八、AI 工具审计日志约定

> 散落在 phases/0.11 §四 + phases/0.13，此处归一。

- **记录表**：`ai_tool_audit`（`blink_ai.db`）
- **必含字段**：`caller`——标识调用来源（主窗口 AI / 对话窗口 Agent / CLI / MCP server），用于审计不同出口的工具调用
- **生命周期**：30 天 `cleanup_old` + 行数上限 10000，防止无限膨胀
- **记录内容**：tool 名 / 参数摘要 / 结果摘要 / 耗时 / caller，**不记敏感原文**（遵循 §三日志铁则）

---

## 九、UTF-8 与字符串安全

### 9.1 脚本插件 UTF-8 强制

**Python**（脚本开头，任何读写之前）：
```python
sys.stdin.reconfigure(encoding='utf-8', errors='replace')
sys.stdout.reconfigure(encoding='utf-8', errors='replace', line_buffering=True)
sys.stderr.reconfigure(encoding='utf-8', errors='replace')
```

**Node.js**（readline 必须显式 encoding）：
```javascript
const rl = readline.createInterface({
  input: process.stdin, output: process.stdout,
  terminal: false, encoding: 'utf8'
});
```

### 9.2 禁止危险字节切片

```toml
[workspace.lints.clippy]
string_slice = "deny"  # 直接按字节切片字符串 → 编译失败
```

字符串截断用安全 API（`src/text.rs::truncate_chars`，按字符数截断），不用裸 `[..n]`。
