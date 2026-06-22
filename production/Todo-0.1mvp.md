# Blink 0.1 MVP — 实现总结

> 技术栈：Rust 2024 edition + Tauri 2.x + WebView2

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

- 搜索缓存（当前每次调用都重新扫描开始菜单）
- 拆分为独立搜索引擎：开始菜单搜索、快捷方式搜索、文件搜索、意图识别
- 每个引擎独立缓存/增量更新，commands 层只做路由和结果合并
- UWP/Store 应用扫描（`PackageManager`）
- `.lnk` 深度解析（COM `IShellLinkW`，获取真实 exe 路径/图标）

### 配置与持久化

- TOML 配置文件读写（快捷键阈值、搜索路径等可配）
- 快捷键阈值滑块实际修改热键参数（当前只改显示值）
- 配置热更新（改快捷键/启用插件无需重启）

### 调试与测量

- 前端调试面板接入实时数据（invoke/show/focus 耗时 + 成功率）
- 1000 次焦点统计脚本

### 多显示器与 DPI

- 多显示器跟随验证（待补测）
- per-monitor DPI aware 显式配置

### 打包与分发

- `cargo tauri build` 打包（msi/nsis）
- WebView2 运行时依赖处理（Win11 自带 / Win10 bundled）
- 代码签名（OV/EV 证书，分发前必须）
- Defender 排除提示（全局键盘钩子会被盯上）

### 更多功能

- 文件搜索（常用目录、最近文件）
- 插件系统（P2）：独立进程 + stdin/stdout JSON
- AI 能力（P3）：规则 → 本地模型（可选插件）→ 云模型
- 语音（P4）：长按唤起录音 → VAD → STT
- Context 层（环境感知：前台应用/选中文本/活跃上下文）
- Proactive 主动建议（基于 context 预测用户意图）
- i18n 多语言支持（当前中文硬编码）
- egui 迁移评估（Launcher 窗口用 egui 替代 WebView2）
