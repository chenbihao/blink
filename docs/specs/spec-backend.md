# 后端实现规范

> **怎么做（HOW）**——Rust 后端代码的硬约束 / 铁则。写或改后端代码前先读本文。
>
> 架构层铁则（分层依赖方向、四域边界、Capability 唯一原子执行、Interaction/ResultAction）见 `./spec-architecture.md`；本卷聚焦编码、测试、日志、错误、事件、存储这些横切工程规范。

---

## 一、编码约定

| 规则 | 说明 |
|---|---|
| **配置化优先** | 可选行为（默认值用户可能想改的）做成配置项 + 合理默认；纯内部参数不暴露 |
| **平台抽象预留** | 平台相关逻辑走 `infra/platform/` 的 `mod.rs` 接口 + `windows.rs` 实现，domain 不直接 `use windows::` |
| **不过度工程** | 0.x 阶段不对外发布，产品化基础设施（manifest 升级/权限强制/插件市场）1.0 前不做 |
| **架构要有前瞻性** | 精心设计持续演进，不过早腐败，不随便堆砌坏味道与技术债，持续收敛 Clean Architecture |
| **子进程静默** | 调外部进程（`netstat`/`taskkill`/`python`/`node` 等子进程）必须加 `CREATE_NO_WINDOW`，走 `infra/platform/process.rs` 的 `no_window()` / `no_window_tokio()`，**禁止裸 `Command`**——否则会闪命令行黑窗。0.17.0 自启链路曾因此泄漏 cmd 窗口 |
| **阻塞操作隔离** | CPU 密集或同步阻塞操作（PNG 编解码、图像像素 swap、Win32 同步 API、SQLite 长查询）必须 `tokio::task::spawn_blocking` 挪出工作线程，**禁止在 async 上下文裸跑**——否则阻塞 tokio 调度器影响 Alt+Space 主链路。参考 0.11 截图 PNG 解码 / BGRA swap / 剪贴板写入均走 `spawn_blocking` |
| **AI 请求单活跃 + 串行启动** | AI 请求必须满足三约束：① 全局单活跃请求（`RequestTracker` 单槽，新请求 abort 旧或返回 `AlreadyActive`）；② `start_gate` 串行化启动，防并发 IPC 绕过 active 检查；③ 0.17.6 后**跨窗口单活跃**——主窗口 AI 运行时对话窗口发消息得 `AlreadyActive`，前端提示"AI 正在 {active_window} 中处理"。防并发请求导致状态错乱（0.12 §3.2 + 0.17.6） |
| **Hook 热路径无锁无 IO** | `WH_KEYBOARD_LL` 回调在系统线程上同步执行，**禁止**取得可能阻塞/竞争的 Mutex/RwLock、查 DB、调 Tauri、`await`、或任何阻塞 IO--否则会拖慢全局键盘响应甚至被系统摘除 Hook。同步 `Pass/Swallow` 判定在回调内完成，业务副作用通过非阻塞 channel send 转交 `HotkeyService` 在 Tauri runtime 执行。原子操作和非阻塞 channel send 允许。0.18.7 进一步把 Hook 决策收敛到单线程 `InputStateMachine` reducer，回调只负责归一化事件 + 同步传播判定（见 [phases/0.18 §3.7 单线程输入引擎](../phases/0.18-enhancement-chord.md)） |

> 分层依赖方向的硬约束（domain 不 use tauri / infra 不反向依赖 app）见 `spec-architecture.md §A1`。

---

## 二、测试策略（务实 TDD）

- ✅ **纯逻辑/算法必须有单测**：计算、fuzzy/拼音、PNG 编码、状态机等可纯函数化的逻辑。主动把可测逻辑从平台调用里抽出来
- ❌ **Win32/GUI/Shell/Tauri 集成层免自动化**：这类调用难以稳定 mock，靠 `cargo run` 手动验证主链路
- ⚠️ **依赖系统资源的测试要可跳过**：用 `Path::exists` 守卫，缺失则跳过（不依赖 CI 桌面环境）
- ✅ **验证产物正确性**：例如断言 PNG 魔数，而不只是 `!is_empty()`
- 🚫 **单测绝不修改真实系统状态**：Credential Manager / 注册表 / 用户文件系统等共享系统资源，单测必须用 mock store 或 `Path::exists` 守卫，**禁止直接打真实 CM**。0.17.11 教训——`cargo test` 的 `enumerate_and_delete_all_blink_secrets` 曾直接清空用户真实密钥，改用 `keyring` mock store 后才根治。secret 相关单测优先 `#[ignore]` + 独立 store，绝不依赖"开发者机器上没数据"这种假设

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

**铁则**：前端 invoke **单一 import 来源** = `frontend/js/shared/tauri.js`。

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

### 7.1 四库数据分类与清理铁则

> 0.17.0 教训：`clear_cache_db` 曾误清用户剪贴板图片；增量清理路径只 DELETE 不 VACUUM 导致 DB 文件不缩小。此处归一清理边界。

| 性质 | 表 | 清理入口 |
|---|---|---|
| **配置（不可清理）** | `config.db` 的 `config`（KV）/ `ai_permission_memory` | 无。铁则：任何"清理缓存"/"一键清理"操作**不得触碰 config.db**；`ai_permission_memory` 仅"清除所有权限记忆"按钮 |
| **用户数据** | `history` / `clipboard_history` / `sticky_notes`（history 库）；`clipboard_images`（cache 库，**用户数据非缓存**）；`conversations` / `messages` / `conversation_groups`（ai 库） | 各自 `clear_*`（带确认）；`sticky_notes` 无清理入口 |
| **缓存（可重建）** | `performance_metrics` / `icon_cache`（cache 库） | `clear_cache_db` / `clear_perf_data` |
| **审计** | `ai_tool_audit`（ai 库） | `clear_ai_audit`（带确认） |

**清理铁则**：
1. `clear_cache_db` 只清缓存表（`performance_metrics` + `icon_cache`），**不碰 `clipboard_images`**——后者是用户数据（0.17.0 曾误归缓存，导致历史图片被清掉）
2. `optimize_storage`（VACUUM）可对 config.db 执行（只回收空闲页，不删数据），与"不得清理 config.db"不冲突
3. `cleanup_all_data`（删整个 `%APPDATA%\blink`）**运行时禁用**——该操作删配置 + 全部用户数据，运行中执行会进入不可恢复状态，仅卸载前手动启用

### 7.2 SQLite VACUUM 策略

不用 `PRAGMA auto_vacuum`（需建表时设置且不还给 OS），用定期 VACUUM：
- 启动时 `vacuum_if_needed(pool, 0.2)`（空闲页占比 > 20% 才 VACUUM）
- 存储页 `optimize_storage` 手动触发四库 VACUUM
- 增量清理路径只 `DELETE` 不 `VACUUM` 是常态（SQLite `DELETE` 只标记空闲页不缩文件是正常的），由上述两个入口按需回收

### 7.3 大 BLOB 独立表/独立库

单张截图 `CF_DIB` ~14MB / PNG 1-3MB。大 BLOB **不进** `max_items` 大的混合表（max_items=10000 可达 20GB），且 SQLite 单 pool 写锁会阻塞文本历史。`clipboard_images` 走 cache 库的独立表，与 `clipboard_history` 文本表分库分管。采集时同步生成缩略图（max 边 256px），避免列表滚动重复解码。

### 7.4 大资源跨边界传输（强制）

- 大型图片、音频及其他二进制数据**不得走 JSON 数组或 Base64 热路径**；优先使用二进制响应、自定义协议或受控资源引用
- 跨 IPC / Capability / MCP 边界时避免完整数据的重复复制、重复编解码和无条件全量加载；可按显示器、视口或页面消费时优先懒加载
- 短期二进制资源使用带 TTL、单项上限和总容量上限的引用；可重放文本使用文件引用 + 有界分页。禁止用一个无界通用 stash 包办所有数据类型
- TTL、容量与分页大小属于实现/config 参数，按真实负载调整，不在规范中固化

### 7.5 持久化对象的并发修改（强制）

可能被 UI、Capability、MCP 等多个入口同时修改的持久化对象，必须携带 revision、`updated_at` 或等价版本并执行乐观并发校验；版本冲突应返回可识别错误，禁止静默覆盖较新的状态。显示/隐藏、删除/回收站等正交状态应分字段表达，不得压缩成一个含混状态。

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

---

## 十、状态接口与长驻运行时

- **只读查询无副作用**：status/snapshot/list 等查询不得因读取而启动进程、建立连接、修复状态或改变 generation；需要恢复时提供显式 action/ensure 入口
- **创建与重连 single-flight**：同一逻辑资源的并发创建/重连只能有一个执行者，其余调用等待并复用结果；失败必须释放创建态，允许后续重试
- **generation 隔离**：MCP client/server、外部进程和其他长驻资源的后台任务必须绑定 generation；旧 generation 的结果、清理和断线事件不得修改新实例
- **缓存覆盖全部行为维度**：凡会改变工具集合、权限、系统提示词或执行模式的配置，必须进入缓存键或触发显式失效，禁止复用语义已过期的运行时对象
