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
    "panel.engines": "内置引擎",
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
    "general.history_enabled.label": "记录搜索历史",
    "general.history_enabled.hint": "关闭后不再记录频率权重",
    "general.history_days.label": "历史保留天数",
    "general.history_days.hint": "0 = 永久保留",
    "general.max_results.label": "最大结果数",
    "general.max_results.hint": "搜索融合后返回上限",

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
    "engine.file_search.title": "文件搜索",
    "engine.file_search.desc": "Everything 极速全盘文件搜索",
    "engine.everything.status.label": "Everything 状态",
    "engine.probe": "重新探测",
    "engine.everything.port.label": "HTTP 端口",
    "engine.everything.port.hint": "Everything 选项 → HTTP 服务器",
    "engine.everything.max_results.label": "最大结果数",
    "engine.everything.max_results.hint": "每次检索返回的文件数量上限",
    "engine.save": "保存配置",
    "engine.status.probing": "探测中…",
    "engine.status.available": "可用 ✓",
    "engine.status.unavailable": "不可用 ✗",
    "engine.status.failed": "探测失败",

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
    "debug.perf.hint": "（搁置，待实现）",
    "debug.perf.invoke": "唤起延迟",
    "debug.perf.show": "show 耗时",
    "debug.perf.focus": "focus 耗时",
    "debug.perf.rate": "成功率",

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
    "menu.resetHistory": "重置该项记录",
    "menu.copyResult": "复制结果",

    // ── 主窗口：提示栏 ──
    "hint.open": "打开",
    "hint.copy": "复制结果",
    "hint.fallback": "执行",
    "hint.enter": "Enter {label}",
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
    "panel.engines": "Built-in Engines",
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
    "general.history_enabled.label": "Record search history",
    "general.history_enabled.hint": "Disables frequency weighting when off",
    "general.history_days.label": "History retention (days)",
    "general.history_days.hint": "0 = keep forever",
    "general.max_results.label": "Max results",
    "general.max_results.hint": "Cap after fusion",

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
    "engine.file_search.title": "File Search",
    "engine.file_search.desc": "Fast full-disk search via Everything",
    "engine.everything.status.label": "Everything status",
    "engine.probe": "Re-probe",
    "engine.everything.port.label": "HTTP port",
    "engine.everything.port.hint": "Everything options → HTTP server",
    "engine.everything.max_results.label": "Max results",
    "engine.everything.max_results.hint": "Cap per query",
    "engine.save": "Save",
    "engine.status.probing": "Probing…",
    "engine.status.available": "Available ✓",
    "engine.status.unavailable": "Unavailable ✗",
    "engine.status.failed": "Probe failed",

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
    "debug.perf.title": "Performance stats",
    "debug.perf.hint": "(deferred)",
    "debug.perf.invoke": "Summon latency",
    "debug.perf.show": "show time",
    "debug.perf.focus": "focus time",
    "debug.perf.rate": "Success rate",

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
    "menu.resetHistory": "Reset this item's history",
    "menu.copyResult": "Copy result",

    // ── Main window: status bar ──
    "hint.open": "Open",
    "hint.copy": "Copy result",
    "hint.fallback": "Run",
    "hint.enter": "Enter {label}",
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
