# Blink 0.1 MVP — 实现总结

> 技术栈：Rust 2024 edition + Tauri 2.x + WebView2
>
> **0.2 进展**：架构演进见 `0.2-core-plugin-design.md`；下方「后期待办」中标 ✅ 的已完成（0.2.0）。

---

## 已完成 ✅

### P0 — 基础交互可靠性

- **全局热键**：右 Alt tap/hold 状态机（`WH_KEYBOARD_LL`），不影响系统组合键
- **悬浮窗口**：无边框、置顶、弹性大小（输入框 + 结果列表动态展开）
- **焦点管理**：看门狗轮询 + 500ms grace period，覆盖焦点抖动和不发 deactivate 的窗口（如 IDEA 终端）
- **显示器定位**：跟随前台窗口所在显示器，屏幕正中居中
- **IME 支持**：中文输入法（微软拼音/搜狗/微信）已验证
- **ESC/失焦隐藏**：全局 ESC 捕获 + 看门狗失焦检测

### P1 — 搜索与计算

- **应用搜索**：扫描开始菜单 `.lnk`，nucleo fuzzy 匹配 + 拼音首字母
- **历史权重**：SQLite 记录执行次数，log 加权排序，常用应用排前面
- **实时计算**：evalexpr 表达式求值（四则运算、括号、取余），整数转浮点避免截断
- **计算结果**：回车自动复制到剪贴板

### 基础设施

- **单实例**：`tauri-plugin-single-instance`，重复启动唤起已有实例
- **托盘**：常驻后台，右键菜单（设置/退出）
- **设置页面**：5 个 Tab（通用/快捷键/存储/调试/关于）
- **代码结构**：main / commands / hotkey / window_ctl / search / calc / history

---

## 后期待办 ⬜

### 搜索引擎拆分

- ~~搜索缓存~~ ✅ 已完成（0.2.0，`search/cache.rs`：内存索引 + 后台预扫 + mtime 增量失效）
- 拆分为独立搜索引擎：开始菜单搜索、快捷方式搜索、文件搜索、意图识别
- 每个引擎独立缓存/增量更新，commands 层只做路由和结果合并
- UWP/Store 应用扫描（`PackageManager`）
- `.lnk` 深度解析（COM `IShellLinkW`，获取真实 exe 路径/图标）

### 配置与持久化

- TOML 配置文件读写（快捷键阈值、搜索路径等可配）
- ~~快捷键阈值滑块实际修改热键参数~~ ✅ 已完成
- ~~配置热更新~~ ✅ 已完成（改快捷键/阈值运行时生效，无需重启）

### 调试与测量

- 前端调试面板接入实时数据（invoke/show/focus 耗时 + 成功率）
- 1000 次焦点统计脚本
- **图标缓存无界增长**（`search/icon.rs` `ICON_CACHE`）：只增不减的 HashMap，每图标几 KB；
  几百应用约几 MB，当前无害但违反「缓存」语义。后续可加 LRU 上限或随搜索缓存刷新一起清理。
- ~~搜索响应延迟~~：已实测定位——后端 `search_apps` 仅 ~1ms（283 条，get_entries/weights/fuzzy 全亚毫秒），
  瓶颈是前端 150ms 防抖，已下调至 40ms。clone entries 仅 0.09ms，无需 Arc 化。
  剩余可选项：图标按可见项加载（当前一次性请求 top-10）。

### 代码质量 / 待清理（review 9bab7fb 记录）

- **热键 `ll_proc` 结构重构**：单函数 ~160 行塞满状态机逻辑，每加特殊处理都要重过整个机；
  0.2.1 Service 化时拆成 `State::on_modifier_down/up`、`on_key_down/up`，并为纯逻辑
  （`is_hotkey_match`、合成事件判定）补单测——热键是 P0 核心却零测试。
- **热键双补丁可精简**：`merge_physical_modifiers`（GetAsyncKeyState 物理补全，桌面场景）与
  合成事件时序过滤（IDEA 场景）对同一问题用两套机制，时序过滤可能已覆盖前者；补测试后评估精简。
- `percent_decode`（`main.rs`）补单测（纯逻辑：非法 % / 短 % / UTF-8 / 路径编码边界）。
- `calc.rs::ints_to_float` 不支持前导点小数 `.5+1`（转成 `.5.0` 失败）；罕见，待定是否支持。
- `statusbar.js` 直接 getElementById，未走 `dom.js` 集中引用（一致性）。
- `icon.rs` 图标提取无 in-flight 去重，并发首次请求同一图标会重复进 Shell/GDI（功能正确，启动尖峰时可优化）。

### 多显示器与 DPI

- 多显示器跟随验证（待补测）
- per-monitor DPI aware 显式配置

### 打包与分发

- `cargo tauri build` 打包（msi/nsis）
- WebView2 运行时依赖处理（Win11 自带 / Win10 bundled）
- 代码签名（OV/EV 证书，分发前必须）
- Defender 排除提示（全局键盘钩子会被盯上）

### 更多功能

- 结果列表底部**提示/状态栏层**：✅ 已做动作提示（Enter 打开/复制，由 action.kind 驱动）+ 翻页提示
  （PgUp/PgDn）。后续增强：随上下文联动的智能提示、更新提示、多动作（Cmd+K 次动作菜单）。
- `action` 字段 0.2.2 并入 `SearchItem::action`（带 payload 的 `SearchAction`，见 0.2 设计 §2.1），
  扩展 Plugin/Ai 等 kind。
- 结果项描述副行**真实 exe 路径**：当前副行显示 lnk 路径，后续用 COM `IShellLinkW` 解析
  lnk 指向的真实 exe 路径（懒解析，不进扫描热路径）。
- 文件搜索（常用目录、最近文件）
- 插件系统（P2）：独立进程 + stdin/stdout JSON
- AI 能力（P3）：规则 → 本地模型（可选插件）→ 云模型
- 语音（P4）：长按唤起录音 → VAD → STT
- Context 层（环境感知：前台应用/选中文本/活跃上下文）
- Proactive 主动建议（基于 context 预测用户意图）
- i18n 多语言支持（当前中文硬编码）
- egui 迁移评估（Launcher 窗口用 egui 替代 WebView2）
