# Blink

> Windows 全局快捷入口。不是启动器，是「Universal Action Layer（统一操作层）」。

**任何操作比原来的路径更快。**

## 愿景

Alfred / Raycast 证明了：一个全局唤起的输入框，可以成为用户与系统交互的「第一入口」。Blink 要在 Windows 上做到同样的事——但不止于此。

终极目标是**感知用户上下文、主动推荐动作**：

```
用户按快捷键
    ↓
自动知道用户当前在看什么
自动知道用户选中了什么
自动知道用户在哪个应用里
    ↓
智能推荐动作，零输入即可执行
```

## 当前能力（0.1 MVP）

- **全局热键**：右 Alt 单击唤起，不影响系统组合键
- **应用搜索**：扫描开始菜单，支持拼音首字母（`wx` → 微信）
- **实时计算**：输入 `1+1` 直接显示 `2`，回车复制结果
- **智能排序**：常用应用自动排前面（历史权重加权）
- **中文输入法**：完美支持微软拼音、搜狗、微信输入法
- **失焦自动隐藏**：点击其他窗口自动隐藏，不干扰工作流
- **弹性窗口**：默认只有输入框，有结果时才展开

## 使用

1. 启动后常驻托盘（无可见窗口）
2. **右 Alt 单击** → 唤起输入框
3. 输入应用名（支持拼音首字母，如 `wx` → 微信）
4. 输入算术表达式（如 `1+1`、`100*0.25`）
5. **上下箭头** 选择结果，**回车** 启动/复制
6. **ESC** 或点击其他地方 → 隐藏

## 开发

```bash
# 环境要求：Rust 1.75+、MSVC Build Tools、WebView2 Runtime

# 开发运行（debug 模式，保留控制台日志）
cargo run

# 打包（需要先安装 tauri-cli）
cargo install tauri-cli
cargo tauri build
```

## 技术栈

| 层 | 选型 | 说明 |
|---|---|---|
| 框架 | Rust 2024 + Tauri 2 + WebView2 | 极低延迟、低内存 |
| 全局热键 | `windows` crate，`WH_KEYBOARD_LL` | tap/hold 状态机，不影响系统键 |
| 搜索 | `nucleo` + `pinyin` | fuzzy 匹配 + 拼音首字母 |
| 计算 | `evalexpr` | 四则运算、括号、取余 |
| 数据 | SQLite + `sqlx` | 历史记录、频率权重 |

## 项目结构

```
src/
├── main.rs        # Tauri 初始化 + 托盘 + 热键启动
├── commands.rs    # Tauri command 层（前端 invoke 入口）
├── hotkey.rs      # 全局热键（右 Alt tap/hold）
├── window_ctl.rs  # 窗口显隐 + 看门狗失焦检测
├── search.rs      # 应用搜索（扫描 + fuzzy + 拼音）
├── calc.rs        # 实时计算（evalexpr）
└── history.rs     # 历史记录（SQLite + 频率权重）
```

## 路线图

- [ ] 搜索引擎拆分（开始菜单/文件/意图识别）
- [ ] 插件系统（独立进程 + stdin/stdout JSON）
- [ ] AI 能力（本地模型 + 云模型）
- [ ] 语音输入（VAD + STT）
- [ ] Context 层（环境感知）
- [ ] Proactive 主动建议
- [ ] i18n 多语言

## 许可

MIT
