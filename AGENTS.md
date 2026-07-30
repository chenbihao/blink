# AGENTS.md

本文件为各类智能体在本仓库工作时提供指引。**指令优先级高于默认行为，必须严格遵循。**

> 📖 **文档入口**：[docs/README.md](docs/README.md)（文档体系总览）→ [docs/product.md](docs/product.md)（产品是什么、为什么）→ [docs/specs/](docs/specs/)（怎么做·铁则）→ [docs/phases/](docs/phases/)（各版做了什么）。

更新时间 20260730

---

## 0. 工作流路由（改代码前先读对应规范）

不同改动类型，开工前**必读**对应文档，否则会违反既定铁则：

| 你要做的 | 开工前必读 |
|---|---|
| **改前端代码**（HTML/CSS/JS） | [docs/specs/spec-frontend.md](docs/specs/spec-frontend.md)（CSS 七层 / token / 中文不斜体 / 图标用包禁 emoji / 竞态防护 / 无 bundler 铁则） |
| **改后端代码**（Rust） | [docs/specs/spec-backend.md](docs/specs/spec-backend.md)（日志分级 / 错误处理 / 事件名常量化 / 测试策略 / UTF-8 安全） |
| **改架构 / 做重构 / 定位代码归属** | [docs/specs/spec-architecture.md](docs/specs/spec-architecture.md)（分层依赖方向 / 四域边界 / Capability·Action 边界 / 信任边界） |
| **新建 phase / 维护 phase 文档** | [docs/specs/spec-phase.md](docs/specs/spec-phase.md)（8 结构块模板 / 子版本切分 / 完成后精简规则） |
| **动核心逻辑**（搜索/路由/能力/Chord） | 对应 [docs/phases/](docs/phases/)（0.2/0.3 标"改核心前必读"，0.8 §五四域架构） |
| **了解产品为什么这么设计** | [docs/product.md](docs/product.md)（定位 / 交互 / 扩展 / 感知 / 原则） |

**铁则**：这些文档是决策的 single source of truth。改动前先读，能避免违反既定铁则（如 domain 不 use tauri、中文不斜体、前端无 bundler）。

---

## 1. 核心目标（最重要）

> **如果用户按快捷键后不能立即输入，其他所有功能都没有意义。**

所有改动都应服务于这条主链路的可靠性：
`Alt+Space → 窗口出现 → 自动 Focus → 用户直接输入 → ESC/失焦隐藏`。

| 指标 | 目标 |
|---|---|
| 快捷键唤起延迟 | &lt; 50ms |
| 输入首个结果延迟 | &lt; 20ms |
| 常驻内存 | &lt; 300MB（Tauri + WebView2 基线约 80-150MB） |
| 输入焦点成功率 | &gt; 99.9% |

---

## 2. 技术栈与构建

| 层 | 技术 |
|---|---|
| 框架 | Tauri 2（Rust 后端 + WebView2 前端） |
| 后端 | Rust 2024、SQLite（`sqlx`）、`tokio`、`tracing` |
| 前端 | 纯静态 HTML/CSS/JS，**无 bundler、无 npm、无构建步骤** |
| 平台 | `windows` crate 直接调 Win32（热键 hook、窗口、Shell 图标、UIA） |

```bash
cargo tauri dev          # 开发（debug，控制台 tracing，默认 error 级；设置页可调）
cargo xtask release      # 打包（= 编译插件 + cargo tauri build；需先 cargo install tauri-cli）
cargo test --bin blink   # 跑单测（bin crate，无 lib target）
```

---

## 3. 关键业务决策（无法从代码推断）

这些是影响实现取舍的架构级约束：

| 决策 | 说明 |
|---|---|
| **热键默认不吞键** | hook 回调全程 `CallNextHookEx` 放行，Alt 仍可作系统修饰键。tap/hold 靠按压时长 + 期间是否出现其他键区分。**例外**：chord 独占模式下，主窗 Alt hold 时吞 chord 键 keydown（仅字母键），避免与其他软件 Alt+A 冲突；退出 chord mode 即恢复放行 |
| **看门狗失焦检测** | 不依赖 `WM_ACTIVATE`，每 150ms 轮询 `GetForegroundWindow()`，按**进程 PID** 判定（非死比 HWND） |
| **搜索双路匹配** | 同时对原始名和拼音首字母做 nucleo fuzzy 取最高分；历史 `ln(hit+1)*0.3` 加权（上限 0.8） |
| **图标懒加载** | 图标提取**不进搜索热路径**，由自定义协议 `blink-icon` 按需提供 |
| **lnk_path 是 history 主键** | 扫描产生的路径字符串不可随意归一化/改写，否则历史权重 key 失配 |

> 架构级决策（四域、Capability/Action 边界、分层依赖方向、信任边界）见 [docs/specs/spec-architecture.md](docs/specs/spec-architecture.md)。

---

## 4. 编码与工程规范（指针）

以下规范已迁入 specs，本文件不再复述——**开工前读对应 spec**：

| 规范 | 位置 |
|---|---|
| 编码约定（配置化优先 / 平台抽象预留 / 不过度工程 / 架构前瞻性） | [spec-backend.md §一](docs/specs/spec-backend.md) |
| 日志规范（tracing 分级 / 结构化 / 错误带上下文 / 敏感信息不记） | [spec-backend.md §三](docs/specs/spec-backend.md) |
| 测试策略（务实 TDD / 集成层免自动化 / 产物正确性） | [spec-backend.md §二](docs/specs/spec-backend.md) |
| 错误处理（thiserror / CapabilityError serde / 插件四层兜底） | [spec-backend.md §四](docs/specs/spec-backend.md) |
| 事件名常量化 / invoke 路径收敛 | [spec-backend.md §五/§六](docs/specs/spec-backend.md) |
| 数据存储（SQLite 四库） | [spec-backend.md §七](docs/specs/spec-backend.md) |
| 前端 CSS 七层 / token / 主题 / 图标 / 视觉交互铁则 | [spec-frontend.md](docs/specs/spec-frontend.md) |
| 分层架构 / 四域 / Capability·Action / 信任边界 | [spec-architecture.md](docs/specs/spec-architecture.md) |

**仍在本文件有效的通用工作铁则**：

| 规则 | 说明 |
|---|---|
| **改完自审** | 每次完成改动后自己 review（diff / 编译 / 副作用）再报告 |
| **关键节点打日志** | 关键节点需要打日志，量适中等级合适；开发流程可打临时日志排查，收尾时清理 |

---

## 5. 模块速查（指针）

源码分层与模块拆分的完整说明见 [spec-architecture.md §A1](docs/specs/spec-architecture.md)。速查：

- `src/main.rs` — Tauri 启动 + 托盘 + 服务 wiring
- `src/app/` — 应用层（commands / config / ai_config / stt_config / voice）
- `src/domain/` — 业务域（context / intent / search / execution / plugin / chord / ai / stt / capability）—— **框架无关，不 use tauri（0.15 收敛中）**
- `src/infra/` — 基础设施（platform / data / utils）—— 最底层，不反向依赖
- `src/cli/` — 自身 CLI 化（mcp-server / search / run / chat）
- `frontend/` — 纯静态前端（主窗口 / 设置页 / 对话窗口 / 截图 overlay / 语音 overlay / 悬浮球 / 右键菜单）

**根目录**（非源码，勿与源码模块混淆）：

| 目录 | 用途 | 易混淆点 |
|---|---|---|
| `capabilities/` | Tauri ACL 权限真源（`*.json`） | ≠ `src/domain/capability/`（业务域能力抽象） |
| `gen/schemas/` | Tauri 自动生成的 IPC Schema | 勿手改，由 `tauri build` / IDE 插件生成 |
| `icons/` | 安装包图标（`.ico` / `.png`） | ≠ `frontend/assets/icons/`（前端 SVG sprite，由 `cargo xtask icons` 生成） |
| `xtask/` | Rust workspace 构建编排入口（`cargo xtask <plugins\|copy\|release\|icons>`） | 脚本如 `xtask/scripts/fetch-lucide-icons.py` 归此管理 |
| `resources/` | 随 Rust 二进制发布的运行时资源（`include_str!` 嵌入） | 只存产物级资源，不接纳开发脚本或生成文件 |
| `plugins/` | 插件源码与 manifest（builtin + examples） | 编译产物 `bin/` 仅 release 时生成 |

前端用 `invoke()` 调 Rust commands，用 `TAU.event.listen()` 监听后端事件（`blink://*`）。

---

## 6. 文档变更回写约定

**任何文档变更回写需用户确认。** 文档是决策的 single source of truth，改动影响后续所有人，不能静默修改。

文档运作规则（三层分工 / 新增决策怎么分流 / phase 生命周期 / 引用约定）见 [docs/README.md §五](docs/README.md)。
