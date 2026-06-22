# Blink

Windows 全局快捷入口工具。类似 Alfred / Raycast，但不止是启动器——是一个「Universal Action Layer（统一操作层）」。

## 特性

- **极低延迟唤起**：右 Alt 单击唤起，<50ms 响应
- **应用搜索**：扫描开始菜单，支持拼音首字母匹配（`wx` → 微信）
- **实时计算**：输入 `1+1` 直接显示 `2`，回车复制结果
- **智能排序**：常用应用自动排前面（历史权重加权）
- **失焦自动隐藏**：点击其他窗口自动隐藏，不干扰工作流

## 技术栈

| 层 | 选型 |
|---|---|
| 框架 | Rust 2024 + Tauri 2 + WebView2 |
| 全局热键 | `windows` crate，`WH_KEYBOARD_LL`（tap/hold 状态机） |
| 搜索 | `nucleo`（fuzzy 匹配）+ `pinyin`（拼音首字母） |
| 计算 | `evalexpr` |
| 数据 | SQLite + `sqlx` |

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

## 开发

```bash
# 环境要求：Rust 1.75+、MSVC Build Tools、WebView2 Runtime

# 安装依赖
cargo build

# 开发运行（debug 模式，保留控制台日志）
cargo run

# 打包
cargo tauri build
```

## 使用

1. 启动后常驻托盘（无可见窗口）
2. **右 Alt 单击** → 唤起输入框
3. 输入应用名（支持拼音首字母，如 `wx` → 微信）
4. 输入算术表达式（如 `1+1`、`100*0.25`）
5. **上下箭头** 选择结果，**回车** 启动/复制
6. **ESC** 或点击其他地方 → 隐藏

## 路线图

- [ ] 搜索引擎拆分（开始菜单/文件/意图识别）
- [ ] 文件搜索
- [ ] 插件系统
- [ ] AI 能力（本地模型 + 云模型）
- [ ] 语音输入
- [ ] i18n 多语言

## 许可

MIT
