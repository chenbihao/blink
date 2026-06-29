//! 国际化（i18n）：中英文 UI 文本切换。
//!
//! 纯静态前端、无 bundler，故手写轻量字典方案：
//! - 静态 HTML 文本：元素打 data-i18n / data-i18n-ph / data-i18n-title，applyI18n() 遍历覆盖。
//! - JS 动态文本：调用方直接 t(key, params)（右键菜单、提示栏、卡片动态渲染等）。
//!
//! 语言来源：AppConfig.language（"zh" | "en"），由 applyI18nFromConfig() 异步读取。
//! 降级：t() 找不到 key → 回退 zh → 再回退到 key 本身，永不抛异常。

import { invoke } from "./tauri.js";

/** 当前语言（模块私有）。setLang 修改，t/applyI18n 读取。 */
let currentLang = "zh";

/** zh / en 双语字典。扁平点分 key，两份必须 key 对齐（缺 key 靠 t() 回退兜底）。 */
const DICT = {
  zh: {
    // ── 侧边 Tab ──
    "tab.general": "通用",
    "tab.hotkey": "快捷键",
    "tab.engines": "引擎",
    "tab.plugins": "插件",
    "tab.network": "网络",
    "tab.context": "上下文",
    "tab.storage": "存储",
    "tab.debug": "调试",
    "tab.about": "关于",

    // ── 面板标题（h2）──
    "panel.general": "通用",
    "panel.hotkey": "快捷键",
    "panel.plugins": "插件管理",
    "panel.network": "网络配置",
    "panel.context": "环境感知",
    "panel.storage": "存储",
    "panel.debug": "调试",
    "panel.about": "关于",

    // ── 通用 Tab ──
    "general.auto_start.label": "开机自启",
    "general.auto_start.hint": "开机自动启动",
    "general.language.label": "语言",
    "general.language.hint": "界面语言，切换后立即生效",
    "general.theme.label": "主题",
    "general.theme.auto": "跟随系统",
    "general.theme.light": "浅色",
    "general.theme.dark": "深色",
    "general.theme.hint": "立即生效",

    "general.theme.genshin-kokomi": "心海",
    "general.theme.genshin-fischl": "菲谢尔",
    "general.theme.genshin-nilou": "妮露",
    "general.theme.genshin-ganyu": "甘雨",
    "general.theme.genshin-hutao": "胡桃",
    "general.theme.genshin-diona": "迪奥娜",
    "general.theme.genshin-nahida": "纳西妲",
    "general.theme.genshin-ayaka": "神里绫华",
    "general.theme.genshin-klee": "可莉",
    "general.theme.genshin-sigewinne": "希格雯",
    "general.theme.genshin-sucrose": "砂糖",
    "general.theme.suzume-journey": "铃芽之旅",
    "general.theme.jay-november": "Jay 十一月的萧邦",
    "general.theme.jay-ye-huimei": "Jay 叶惠美",

    "general.history_enabled.label": "记录搜索历史",
    "general.history_enabled.hint": "关闭后不再记录频率权重",
    "general.history_days.label": "历史保留天数",
    "general.history_days.hint": "0 = 永久保留",
    "general.max_results.label": "最大结果数",
    "general.max_results.hint": "搜索融合后返回上限",
    "general.page_size.label": "每页条数",
    "general.page_size.hint": "每屏最多显示多少条结果",

    // ── 快捷键 Tab ──
    "hotkey.label": "唤起快捷键",
    "hotkey.record.title": "点击后按下快捷键",
    "hotkey.reset": "恢复默认",
    "hotkey.reset.title": "恢复默认",
    "hotkey.tap.label": "tap 阈值",
    "hotkey.grace.label": "看门狗 grace period",
    "hotkey.recording": "请按下快捷键...（10秒超时）",
    "hotkey.unit.ms": "{value}ms",

    // ── 引擎 Tab ──
    "panel.engines": "搜索引擎",
    "engine.start_menu.title": "应用搜索",
    "engine.start_menu.desc": "搜索开始菜单、桌面快捷方式等已安装应用",
    "engine.start_menu.config": "配置",
    "engine.start_menu.scan_depth.label": "扫描深度",
    "engine.start_menu.scan_depth.hint": "索引深度，越大越全但越慢",
    "engine.start_menu.include_uwp.label": "包含 UWP/MSIX 应用",
    "engine.start_menu.include_uwp.hint": "搜索 Microsoft Store 安装的打包应用",
    "engine.file_search.title": "文件搜索",
    "engine.file_search.desc": "搜索本地文件，支持 Everything 极速索引",
    "engine.file_search.data_source": "数据源",
    "engine.file_search.data_source.label": "搜索模式",
    "engine.file_search.data_source.auto": "自动（优先 Everything，降级本地）",
    "engine.file_search.data_source.everything": "仅 Everything",
    "engine.file_search.data_source.local": "仅本地扫描",
    "engine.file_search.data_source.hint": "选择文件搜索的数据来源",
    "engine.calc.title": "计算器",
    "engine.calc.desc": "输入数学表达式直接计算结果",
    "engine.builtin_actions.title": "内置动作",
    "engine.builtin_actions.desc": "锁屏、关机、打开设置等系统操作，以及依剪贴板/选区上下文出现的智能动作",
    "engine.builtin_actions.empty": "没有可展示的内置动作",
    "engine.builtin_actions.keywords_label": "关键词",
    "engine.builtin_actions.param_label": "参数来自",
    "engine.builtin_actions.saving": "保存中…",
    "engine.builtin_actions.saved": "已保存",
    "engine.builtin_actions.save_failed": "保存失败",
    "engine.everything.hint": "（可选，需安装）",
    "engine.everything.status.label": "Everything 状态",
    "engine.probe": "重新探测",
    "engine.everything.port.label": "HTTP 端口",
    "engine.everything.port.hint": "Everything 选项 → HTTP 服务器",
    "engine.everything.max_results.label": "最大结果数",
    "engine.everything.max_results.hint": "每次检索返回的文件数量上限",
    "engine.save": "保存配置",
    "engine.local.title": "本地搜索",
    "engine.local.scan_depth.label": "扫描深度",
    "engine.local.scan_depth.hint": "开始菜单索引深度，越大越全但越慢",
    "engine.local.status.label": "缓存状态",
    "engine.status.probing": "探测中…",
    "engine.status.available": "可用 ✓",
    "engine.status.unavailable": "不可用 ✗",
    "engine.status.failed": "探测失败",
    "engine.status.version_low": "版本过低",
    "engine.status.not_found": "未找到",

    // ── 引擎 Tab：脚本解释器（Phase 0.6） ──
    "engine.interpreter.title": "脚本解释器",
    "engine.interpreter.desc": "Python / Node.js 脚本插件运行环境",
    "engine.python.title": "Python",
    "engine.python.status": "当前状态",
    "engine.python.path": "Python 路径",
    "engine.python.path.ph": "自动探测或手动输入...",
    "engine.python.hint": "用于运行 .py 脚本插件，需要 Python 3.8+",
    "engine.node.title": "Node.js",
    "engine.node.status": "当前状态",
    "engine.node.path": "Node 路径",
    "engine.node.path.ph": "自动探测或手动输入...",
    "engine.node.hint": "用于运行 .js 脚本插件，需要 Node.js 16+",
    "engine.browse": "浏览",
    "engine.probe_all": "重新探测全部",
    // spinner 按钮
    "spinner.increase": "增加",
    "spinner.decrease": "减少",
    // 插件触发词
    "plugin.trigger_restore": "恢复触发词",
    "plugin.trigger_disable": "禁用此触发词",
    // 错误消息
    "error.port_range": "端口号必须在 1-65535 之间",
    // 文件对话框
    "file_dialog.python_title": "选择 Python 可执行文件",
    "file_dialog.node_title": "选择 Node.js 可执行文件",
    "file_dialog.exe_filter": "可执行文件",

    // ── 调试 Tab：日志 ──
    "log.level.label": "日志级别",
    "log.level.error": "error（仅错误）",
    "log.level.info": "info",
    "log.level.debug": "debug（详细）",
    "log.level.trace": "trace（最详细）",
    "log.level.hint": "切换立即生效，写入日志文件",
    "log.file.label": "日志文件",
    "log.action.label": "操作",
    "log.open_file": "打开日志",
    "log.open_dir": "打开文件夹",

    // ── 调试 Tab：性能统计 ──
    "debug.perf.title": "性能统计",
    "debug.perf.hint": "（最近 100 次采样）",
    "debug.perf.loading": "加载中…",
    "debug.perf.no_data": "暂无数据",
    "debug.perf.refresh": "刷新",
    "debug.perf.export": "导出报告",
    "debug.perf.exported": "已导出",
    "debug.perf.clear": "清除记录",
    "debug.perf.clear.confirm": "确定清除所有性能统计记录？",
    "debug.perf.cleared": "已清除",
    // 启动耗时
    "debug.perf.startup.title": "启动耗时",
    "debug.perf.startup.total": "总启动时间",
    // 热键唤起
    "debug.perf.hotkey.title": "热键唤起",
    "debug.perf.hotkey.key_to_show": "按键到显示",
    // 搜索引擎
    "debug.perf.search.title": "搜索引擎",
    "debug.perf.search.total": "总搜索耗时",
    // 慢查询
    "debug.perf.slow.title": "慢查询日志",
    "debug.perf.slow.hotkey": "慢热键 (>100ms)",
    "debug.perf.slow.search": "慢搜索 (>200ms)",
    "debug.perf.slow.empty": "无慢查询记录",
    // 统计卡片
    "debug.perf.stats.count": "采样数",
    "debug.perf.stats.p50": "P50",
    "debug.perf.stats.p90": "P90",
    "debug.perf.stats.p99": "P99",
    "debug.perf.stats.min": "最小",
    "debug.perf.stats.max": "最大",
    "debug.perf.stats.avg": "平均",
    // 单位
    "debug.perf.unit.ms": "ms",

    // ── 存储 Tab ──
    "storage.history.label": "历史记录",
    "storage.db_path.label": "数据库路径",
    "storage.clear": "清空历史记录",
    "storage.clear.confirm": "确定清空所有历史记录？",
    "storage.history_count": "{count} 条记录",
    "storage.loading": "加载中…",

    // ── 关于 Tab ──
    "about.version.label": "版本",
    "about.stack.label": "技术栈",
    "about.license.label": "许可",

    // ── 网络 Tab（动态渲染）──
    "network.title": "全局网络代理",
    "network.desc": "本体、网络插件共用，修改后需重启 Blink 生效",
    "network.section": "代理配置",
    "network.http.label": "HTTP 代理",
    "network.http.ph": "http://127.0.0.1:7890",
    "network.https.label": "HTTPS 代理",
    "network.https.ph": "http://127.0.0.1:7890",
    "network.save": "保存配置",
    "network.saved_msg": "已保存，下次查询自动生效",
    "network.save_failed": "保存失败",

    // ── 上下文 Tab（动态渲染）──
    "context.title": "环境感知",
    "context.desc": "唤起时自动采集前台应用、剪贴板等上下文，用于搜索增强",
    "context.clipboard": "采集剪贴板文本",
    "context.sensitive.title": "敏感应用（前台时不采集上下文）",
    "context.sensitive.hint": "如密码管理器、银行软件等，保护隐私",
    "context.add_app": "＋ 添加应用",
    "context.empty": "暂无敏感应用",
    "context.remove.title": "移除",
    "context.auto_saved": "✓ 已自动保存",
    "context.save_failed": "保存失败",
    "context.modal.title": "添加敏感应用",
    "context.modal.search_ph": "搜索进程名…",
    "context.modal.hint": "选择后自动添加并保存",
    "context.modal.done": "完成",
    "context.modal.empty": "没有匹配的进程",
    "context.modal.added": "已添加",

    // ── 插件 Tab（动态渲染）──
    "plugin.desc_default": "暂无描述",
    "plugin.no_trigger": "无触发关键词",
    "plugin.trigger": "触发: {kw}",
    "plugin.no_config": "（该插件无可配置项）",
    "plugin.enabled": "已启用",
    "plugin.disabled": "已禁用",
    "plugin.toggle.title": "启用/禁用插件",
    "plugin.section": "配置",
    "plugin.save": "保存配置",
    "plugin.saved_msg": "已保存",
    "plugin.load_failed": "加载插件列表失败",
    "plugin.empty": "暂无已加载插件",
    // 触发关键字配置
    "plugin.trigger_section": "触发关键字",
    "plugin.trigger_default": "默认",
    "plugin.trigger_custom": "自定义",
    "plugin.trigger_add": "添加",
    "plugin.trigger_add_label": "+ 添加触发词",
    "plugin.trigger_label": "关键词：",
    "plugin.trigger_delete": "删除",
    "plugin.trigger_empty": "（暂无自定义触发词）",
    "plugin.trigger_placeholder": "输入触发词…",
    "plugin.trigger_disable_default": "仅用自定义，禁用默认触发词",
    "plugin.trigger_save": "保存",
    "plugin.unsaved": "待保存",

    // ── 主窗口：右键菜单 ──
    "menu.cut": "剪切",
    "menu.copy": "复制",
    "menu.paste": "粘贴",
    "menu.selectAll": "全选",
    "menu.openSettings": "打开设置",
    "menu.exit": "退出 Blink",
    "menu.open": "打开",
    "menu.openFolder": "打开所在文件夹",
    "menu.openLnkTarget": "打开快捷方式目标",
    "menu.copyPath": "复制路径",
    "menu.copyFullPath": "复制完整路径",
    "menu.copyName": "复制文件名",
    "menu.copyFullName": "复制完整文件名",
    "menu.copyId": "复制应用 ID",
    "menu.resetHistory": "重置该项记录",
    "menu.copyResult": "复制结果",

    // ── 主窗口：提示栏 ──
    "hint.open": "打开",
    "hint.copy": "复制结果",
    "hint.fallback": "执行",
    "hint.enter": "Enter {label}",
    "hint.navigate": "↑↓ 选择",
    "hint.alt_number": "Alt+数字 快捷触发",
    "statusbar.paging": "PgUp/PgDn 翻页 · {page}/{pageCount}",

    // ── 主窗口：搜索框 ──
    "search.placeholder": "输入应用名、计算…",

    // ── Toast / 通用 ──
    "toast.file_search_saved": "文件搜索配置已保存",
    "common.save_failed_msg": "保存失败: {err}",
  },

  en: {
    // ── Sidebar tabs ──
    "tab.general": "General",
    "tab.hotkey": "Hotkey",
    "tab.engines": "Engines",
    "tab.plugins": "Plugins",
    "tab.network": "Network",
    "tab.context": "Context",
    "tab.storage": "Storage",
    "tab.debug": "Debug",
    "tab.about": "About",

    // ── Panel titles (h2) ──
    "panel.general": "General",
    "panel.hotkey": "Hotkey",
    "panel.plugins": "Plugin Manager",
    "panel.network": "Network Settings",
    "panel.context": "Context Awareness",
    "panel.storage": "Storage",
    "panel.debug": "Debug",
    "panel.about": "About",

    // ── General tab ──
    "general.auto_start.label": "Launch at startup",
    "general.auto_start.hint": "Start automatically on boot",
    "general.language.label": "Language",
    "general.language.hint": "UI language, applies immediately",
    "general.theme.label": "Theme",
    "general.theme.auto": "System",
    "general.theme.light": "Light",
    "general.theme.dark": "Dark",
    "general.theme.hint": "Applies immediately",
    "general.theme.genshin-kokomi": "Kokomi",
    "general.theme.genshin-fischl": "Fischl",
    "general.theme.genshin-nilou": "Nilou",
    "general.theme.genshin-ganyu": "Ganyu",
    "general.theme.genshin-hutao": "Hu Tao",
    "general.theme.genshin-diona": "Diona",
    "general.theme.genshin-nahida": "Nahida",
    "general.theme.genshin-ayaka": "Ayaka",
    "general.theme.genshin-klee": "Klee",
    "general.theme.genshin-sigewinne": "Sigewinne",
    "general.theme.genshin-sucrose": "Sucrose",
    "general.theme.suzume-journey": "Suzume",
    "general.theme.jay-november": "Jay - November's Chopin",
    "general.theme.jay-ye-huimei": "Jay - Ye Huimei",
    "general.history_enabled.label": "Record search history",
    "general.history_enabled.hint": "Disables frequency weighting when off",
    "general.history_days.label": "History retention (days)",
    "general.history_days.hint": "0 = keep forever",
    "general.max_results.label": "Max results",
    "general.max_results.hint": "Cap after fusion",
    "general.page_size.label": "Items per page",
    "general.page_size.hint": "Max items displayed per screen",

    // ── Hotkey tab ──
    "hotkey.label": "Summon hotkey",
    "hotkey.record.title": "Click then press keys",
    "hotkey.reset": "Reset to default",
    "hotkey.reset.title": "Reset to default",
    "hotkey.tap.label": "Tap threshold",
    "hotkey.grace.label": "Watchdog grace period",
    "hotkey.recording": "Press a shortcut... (10s timeout)",
    "hotkey.unit.ms": "{value}ms",

    // ── Engines tab ──
    "panel.engines": "Search Engines",
    "engine.start_menu.title": "App Search",
    "engine.start_menu.desc": "Search installed apps from start menu and desktop shortcuts",
    "engine.start_menu.config": "Config",
    "engine.start_menu.scan_depth.label": "Scan depth",
    "engine.start_menu.scan_depth.hint": "Index depth, deeper = more results but slower",
    "engine.start_menu.include_uwp.label": "Include UWP/MSIX apps",
    "engine.start_menu.include_uwp.hint": "Search packaged apps installed from Microsoft Store",
    "engine.file_search.title": "File Search",
    "engine.file_search.desc": "Search local files, supports Everything fast indexing",
    "engine.file_search.data_source": "Data Source",
    "engine.file_search.data_source.label": "Search mode",
    "engine.file_search.data_source.auto": "Auto (Everything first, fallback to local)",
    "engine.file_search.data_source.everything": "Everything only",
    "engine.file_search.data_source.local": "Local scan only",
    "engine.file_search.data_source.hint": "Choose data source for file search",
    "engine.calc.title": "Calculator",
    "engine.calc.desc": "Type math expressions to calculate results",
    "engine.builtin_actions.title": "Built-in Actions",
    "engine.builtin_actions.desc": "System actions like Lock/Shutdown/Open Settings, plus smart actions surfaced by clipboard/selection context",
    "engine.builtin_actions.empty": "No built-in actions available",
    "engine.builtin_actions.keywords_label": "Keywords",
    "engine.builtin_actions.param_label": "Argument from",
    "engine.builtin_actions.saving": "Saving…",
    "engine.builtin_actions.saved": "Saved",
    "engine.builtin_actions.save_failed": "Save failed",
    "engine.everything.hint": "(optional, requires installation)",
    "engine.everything.status.label": "Everything status",
    "engine.probe": "Re-probe",
    "engine.everything.port.label": "HTTP port",
    "engine.everything.port.hint": "Everything options → HTTP server",
    "engine.everything.max_results.label": "Max results",
    "engine.everything.max_results.hint": "Cap per query",
    "engine.save": "Save",
    "engine.local.title": "Local Search",
    "engine.local.scan_depth.label": "Scan depth",
    "engine.local.scan_depth.hint": "Start menu index depth, deeper = more results but slower",
    "engine.local.status.label": "Cache status",
    "engine.status.probing": "Probing…",
    "engine.status.available": "Available ✓",
    "engine.status.unavailable": "Unavailable ✗",
    "engine.status.failed": "Probe failed",
    "engine.status.version_low": "Version too low",
    "engine.status.not_found": "Not found",

    // ── Engines tab: interpreters (Phase 0.6) ──
    "engine.interpreter.title": "Script Interpreters",
    "engine.interpreter.desc": "Python / Node.js runtime for script plugins",
    "engine.python.title": "Python",
    "engine.python.status": "Status",
    "engine.python.path": "Python path",
    "engine.python.path.ph": "Auto-detect or enter manually...",
    "engine.python.hint": "For .py script plugins, requires Python 3.8+",
    "engine.node.title": "Node.js",
    "engine.node.status": "Status",
    "engine.node.path": "Node path",
    "engine.node.path.ph": "Auto-detect or enter manually...",
    "engine.node.hint": "For .js script plugins, requires Node.js 16+",
    "engine.browse": "Browse",
    "engine.probe_all": "Re-probe all",
    // Spinner buttons
    "spinner.increase": "Increase",
    "spinner.decrease": "Decrease",
    // Plugin triggers
    "plugin.trigger_restore": "Restore trigger",
    "plugin.trigger_disable": "Disable this trigger",
    // Error messages
    "error.port_range": "Port must be between 1-65535",
    // File dialog
    "file_dialog.python_title": "Select Python executable",
    "file_dialog.node_title": "Select Node.js executable",
    "file_dialog.exe_filter": "Executable files",

    // ── Debug tab: logging ──
    "log.level.label": "Log level",
    "log.level.error": "error (errors only)",
    "log.level.info": "info",
    "log.level.debug": "debug (verbose)",
    "log.level.trace": "trace (most verbose)",
    "log.level.hint": "Applies immediately, written to log file",
    "log.file.label": "Log file",
    "log.action.label": "Actions",
    "log.open_file": "Open log",
    "log.open_dir": "Open folder",

    // ── Debug tab: performance ──
    "debug.perf.title": "Performance Stats",
    "debug.perf.hint": "(last 100 samples)",
    "debug.perf.loading": "Loading…",
    "debug.perf.no_data": "No data yet",
    "debug.perf.refresh": "Refresh",
    "debug.perf.export": "Export Report",
    "debug.perf.exported": "Exported",
    "debug.perf.clear": "Clear Data",
    "debug.perf.clear.confirm": "Clear all performance metrics?",
    "debug.perf.cleared": "Cleared",
    // Startup
    "debug.perf.startup.title": "Startup Time",
    "debug.perf.startup.total": "Total Startup",
    // Hotkey
    "debug.perf.hotkey.title": "Hotkey Latency",
    "debug.perf.hotkey.key_to_show": "Key to Show",
    // Search
    "debug.perf.search.title": "Search Engine",
    "debug.perf.search.total": "Total Search",
    // Slow queries
    "debug.perf.slow.title": "Slow Query Log",
    "debug.perf.slow.hotkey": "Slow Hotkey (>100ms)",
    "debug.perf.slow.search": "Slow Search (>200ms)",
    "debug.perf.slow.empty": "No slow queries",
    // Stats cards
    "debug.perf.stats.count": "Samples",
    "debug.perf.stats.p50": "P50",
    "debug.perf.stats.p90": "P90",
    "debug.perf.stats.p99": "P99",
    "debug.perf.stats.min": "Min",
    "debug.perf.stats.max": "Max",
    "debug.perf.stats.avg": "Avg",
    // Units
    "debug.perf.unit.ms": "ms",

    // ── Storage tab ──
    "storage.history.label": "History",
    "storage.db_path.label": "Database path",
    "storage.clear": "Clear history",
    "storage.clear.confirm": "Clear all history?",
    "storage.history_count": "{count} records",
    "storage.loading": "Loading…",

    // ── About tab ──
    "about.version.label": "Version",
    "about.stack.label": "Tech stack",
    "about.license.label": "License",

    // ── Network tab (dynamic) ──
    "network.title": "Global Network Proxy",
    "network.desc": "Shared by core and network plugins; restart Blink to apply",
    "network.section": "Proxy settings",
    "network.http.label": "HTTP proxy",
    "network.http.ph": "http://127.0.0.1:7890",
    "network.https.label": "HTTPS proxy",
    "network.https.ph": "http://127.0.0.1:7890",
    "network.save": "Save",
    "network.saved_msg": "Saved, applies to next query",
    "network.save_failed": "Save failed",

    // ── Context tab (dynamic) ──
    "context.title": "Context Awareness",
    "context.desc": "Auto-capture foreground app, clipboard, etc. on summon for search boost",
    "context.clipboard": "Capture clipboard text",
    "context.sensitive.title": "Sensitive apps (no capture when in foreground)",
    "context.sensitive.hint": "e.g. password managers, banking apps; protects privacy",
    "context.add_app": "+ Add app",
    "context.empty": "No sensitive apps",
    "context.remove.title": "Remove",
    "context.auto_saved": "✓ Saved automatically",
    "context.save_failed": "Save failed",
    "context.modal.title": "Add sensitive app",
    "context.modal.search_ph": "Search process name…",
    "context.modal.hint": "Auto-add and save on selection",
    "context.modal.done": "Done",
    "context.modal.empty": "No matching process",
    "context.modal.added": "Added",

    // ── Plugins tab (dynamic) ──
    "plugin.desc_default": "No description",
    "plugin.no_trigger": "No trigger keyword",
    "plugin.trigger": "Trigger: {kw}",
    "plugin.no_config": "(No configurable options)",
    "plugin.enabled": "Enabled",
    "plugin.disabled": "Disabled",
    "plugin.toggle.title": "Enable/disable plugin",
    "plugin.section": "Configuration",
    "plugin.save": "Save",
    "plugin.saved_msg": "Saved",
    "plugin.load_failed": "Failed to load plugins",
    "plugin.empty": "No plugins loaded",
    // Trigger keyword config
    "plugin.trigger_section": "Trigger Keywords",
    "plugin.trigger_default": "Default",
    "plugin.trigger_custom": "Custom",
    "plugin.trigger_add": "Add",
    "plugin.trigger_add_label": "+ Add trigger",
    "plugin.trigger_label": "Triggers: ",
    "plugin.trigger_delete": "Delete",
    "plugin.trigger_empty": "(No custom triggers)",
    "plugin.trigger_placeholder": "Enter trigger word…",
    "plugin.trigger_disable_default": "Custom only, disable default triggers",
    "plugin.trigger_save": "Save",
    "plugin.unsaved": "Unsaved",

    // ── Main window: context menu ──
    "menu.cut": "Cut",
    "menu.copy": "Copy",
    "menu.paste": "Paste",
    "menu.selectAll": "Select all",
    "menu.openSettings": "Open Settings",
    "menu.exit": "Quit Blink",
    "menu.open": "Open",
    "menu.openFolder": "Open containing folder",
    "menu.openLnkTarget": "Open shortcut target",
    "menu.copyPath": "Copy path",
    "menu.copyFullPath": "Copy full path",
    "menu.copyName": "Copy file name",
    "menu.copyFullName": "Copy full file name",
    "menu.copyId": "Copy app ID",
    "menu.resetHistory": "Reset this item's history",
    "menu.copyResult": "Copy result",

    // ── Main window: status bar ──
    "hint.open": "Open",
    "hint.copy": "Copy result",
    "hint.fallback": "Run",
    "hint.enter": "Enter {label}",
    "hint.navigate": "↑↓ Select",
    "hint.alt_number": "Alt+number quick launch",
    "statusbar.paging": "PgUp/PgDn page · {page}/{pageCount}",

    // ── Main window: search box ──
    "search.placeholder": "Type app name, calculate…",

    // ── Toast / common ──
    "toast.file_search_saved": "File search settings saved",
    "common.save_failed_msg": "Save failed: {err}",
  },
};

/** 支持的语言集合（setLang 合法性校验用）。 */
const SUPPORTED = new Set(["zh", "en"]);

/**
 * 取翻译并插值。{name} 占位符用 params[name] 替换（缺失填空串）。
 * 降级链：currentLang → zh → key 本身。永不抛异常。
 * @param {string} key 点分字典 key，如 "menu.copy"
 * @param {Record<string, string|number>} [params] 插值参数
 * @returns {string}
 */
export function t(key, params) {
  const raw = DICT[currentLang]?.[key] ?? DICT.zh[key] ?? key;
  if (!params) return raw;
  return raw.replace(/\{(\w+)\}/g, (_, name) => (params[name] ?? "").toString());
}

/** 读取当前语言。 */
export function getLang() {
  return currentLang;
}

/**
 * 设置当前语言（仅改模块状态，不刷 DOM）。非法值回退 zh。
 * @param {string} lang "zh" | "en"
 */
export function setLang(lang) {
  currentLang = SUPPORTED.has(lang) ? lang : "zh";
}

const ATTRS = [
  ["data-i18n", "textContent"],
  ["data-i18n-ph", "placeholder"],
  ["data-i18n-title", "title"],
  ["data-i18n-aria-label", "ariaLabel"],
];

/**
 * 遍历 DOM 中打了 i18n 标记的元素，按当前语言批量覆盖文本/属性。
 * 带插值的动态文本（如计数、翻页）不能用此法，须调用方自行 t() 重算。
 * @param {string} [lang] 不传则用 currentLang（传入时也会 setLang）
 */
export function applyI18n(lang) {
  if (lang) setLang(lang);
  for (const [attr, prop] of ATTRS) {
    document.querySelectorAll(`[${attr}]`).forEach((el) => {
      el[prop] = t(el.getAttribute(attr));
    });
  }
}

/**
 * 从 AppConfig 读 language 并应用：setLang + applyI18n。
 * 启动时与窗口 shown 刷新时调用。读失败保持 currentLang 默认值。
 */
export async function applyI18nFromConfig() {
  try {
    const cfg = await invoke("get_config");
    if (cfg && cfg.language) setLang(cfg.language);
  } catch (e) {
    console.error("applyI18nFromConfig: 读 config 失败，回退默认语言", e);
  }
  applyI18n();
}
