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
    "tab.context": "上下文感知",
    "tab.chord": "Chord 交互",
    "tab.storage": "存储",
    "tab.debug": "调试",
    "tab.about": "关于",

    // ── 面板标题（h2）──
    "panel.general": "通用",
    "panel.hotkey": "快捷键",
    "panel.plugins": "插件管理",
    "panel.network": "网络配置",
    "panel.context": "上下文感知",
    "panel.chord": "Chord 交互",
    "panel.storage": "存储",
    "panel.debug": "调试",
    "panel.about": "关于",

    // ── 面板 lede（每个 tab h2 下的一句导览）──
    "general.lede": "全局偏好：主题、语言、历史与结果条数上限。",
    "hotkey.lede": "唤起 Blink 的键，以及看门狗判定的时间阈值。",
    "engines.lede": "应用 / 文件 / 计算器 / 内置动作 —— 搜索候选的来源。",
    "plugins.lede": "已安装的第三方与内置插件，逐一开关与配置。",
    "network.lede": "HTTP / HTTPS 代理，本体与所有插件共用一份配置。",
    "storage.lede": "数据库与历史记录的存储位置和清理操作。",
    "debug.lede": "日志级别与性能采样，排查用不到就不用打开。",
    "about.lede": "版本、技术栈与源码仓库。",

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
    "general.autosuggest.title": "输入补全",
    "general.autosuggest.enabled.label": "启用 Ghost Text 补全",
    "general.autosuggest.enabled.hint": "首拼命中时在输入框显示灰色补全提示，按 Tab 接受",
    "general.autosuggest.min_score.label": "模糊阈值",
    "general.autosuggest.min_score.hint": "部分拼音触发 Ghost Text 的最低相似度（0.5~0.95）",
    "general.autosuggest.tab_key.label": "接受补全键",
    "general.autosuggest.tab_key.hint": "按此键把输入替换为规范形式并触发搜索",

    // ── 快捷键 Tab ──
    "hotkey.label": "唤起快捷键",
    "hotkey.record.title": "点击后按下快捷键",
    "hotkey.reset": "恢复默认",
    "hotkey.reset.title": "恢复默认",
    "hotkey.tap.label": "tap 阈值",
    "hotkey.tap.title": "短按到抬起的最长时长，超过则判定为长按（Hold），不触发唤起。按下到抬起若超过此时长，视为长按（保留系统修饰键功能），不触发 Blink。",
    "hotkey.tap.hint": "",
    "hotkey.grace.label": "看门狗 grace period",
    "hotkey.grace.title": "窗口刚显示后的失焦保护期，避免焦点还没切过来就被误判为失焦而隐藏。窗口显示后此段时间内不检测失焦，避免焦点切换尚未完成时被误判隐藏。",
    "hotkey.grace.hint": "",
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
    "debug.section.log": "日志 · 排查现场",
    "debug.section.perf": "性能统计 · 最近采样",
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
    "storage.action.label": "操作",
    "storage.clear": "清空历史记录",
    "storage.clear.confirm": "确定清空所有历史记录？",
    "storage.history_count": "{count} 条记录",
    "storage.loading": "加载中…",

    // ── 关于 Tab ──
    "about.version.label": "版本",
    "about.stack.label": "技术栈",
    "about.license.label": "许可",
    "about.repository.label": "仓库",
    "about.update.check": "检查更新",
    "about.update.checking": "检查中…",
    "about.update.available": "新版本 v{version} 可用",
    "about.update.download": "前往下载",
    "about.update.latest": "已是最新版本",
    "about.update.failed": "检查失败，请稍后重试",

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
    "context.desc": "总开关：关闭后所有采集立即停止",
    "context.lede": "Blink 观察你选中 / 复制 / 剪贴的内容，主动推荐能对它做的事。",
    "context.section.capture": "采集 · 数据从哪来",
    "context.section.filter": "过滤 · 哪些应用不采",
    "context.section.trigger": "触发 · 什么算命中",
    "context.section.present": "呈现 · 用户如何看到",
    "context.filter.title": "敏感应用",
    "context.filter.desc": "前台是这些应用时暂停一切采集，避免密码 / 网银界面被读到",
    "context.trigger.card.title": "Ghost 触发规则",
    "context.trigger.card.desc": "环境自动触发的建议（选中英文→翻译、剪贴板 URL→打开链接 等）；关闭后 Ghost 不再出现",
    "context.autosuggest.title": "输入补全 · Ghost Text",
    "context.bindings.title": "Context 触发规则",
    "context.bindings.empty": "暂无已注册的 Context 触发规则",
    "context.trigger.text_is_non_target_lang": "文本非目标语言",
    "context.trigger.clipboard_is_url": "剪贴板是 URL",
    "context.trigger.clipboard_is_file_path": "剪贴板是文件路径",
    "context.trigger.selection_non_empty": "选区非空",
    "context.clipboard": "采集剪贴板文本",
    "context.selection": "划词感知（选中文本）",
    "context.selection.hint": "鼠标划选文本后自动抓取，唤起 Blink 时作为上下文使用。局限：仅 Windows；依赖应用支持 UIA TextPattern，浏览器（Chrome/Edge/Firefox）、Office、VS Code、原生 Win32 支持较好，部分 Electron 应用（如新版 QQ/微信/Discord）、终端、游戏可能抓不到；无选区时可先复制文本，Blink 会自动读剪贴板。",

    // ── Chord 交互面板（0.8.5.1 §6.6）──
    "chord.lede": "主窗可见且未开始输入时，按住 Alt 可触发快捷动作（区域截图 / 划词翻译 / 剪贴板历史）",
    "chord.general.title": "总控",
    "chord.enabled.label": "启用 Chord",
    "chord.enabled.hint": "关闭后 Alt+字母 不再触发 Chord 动作",
    "chord.hint_visible.label": "显示提示条",
    "chord.hint_visible.hint": "按住 Alt 时是否在输入框内浮现单行动作提示",
    "chord.actions.title": "动作列表",
    "chord.actions.hint": "取消勾选后该 Chord 不再列在提示条，Alt+字母 也不再触发",
    "chord.actions.empty": "暂无已注册的 Chord 动作",
    "chord.section.actions": "动作 · Alt + 字母 直达",
    "chord.action.screenshot.subtitle": "全屏拖选区域，写入剪贴板",
    "chord.action.selection.subtitle": "把当前选中文本抓入 Blink 输入框",
    "chord.action.clipboard_history.subtitle": "打开剪贴板历史召回面板",
    "chord.clipboard.title": "剪贴板历史",
    "chord.clipboard.enabled.label": "监听剪贴板写入",
    "chord.clipboard.enabled.hint": "开启后自动记录复制内容，输入\"剪贴板\"或 Alt+C 召回历史（如启动时为关闭状态，首次打开需重启一次让监听器建立）",

    // ── AI 交互面板（0.9.1 Phase 6）──
    "tab.ai": "AI",
    "panel.ai": "AI 意图辅助",
    "ai.lede": "打字未命中规则时，AI 尝试理解并推荐操作。密钥存 Windows Credential Manager，SQLite 只存别名。",
    "ai.enabled.label": "启用 AI 意图辅助",
    "ai.enabled.hint": "关闭时任何输入都不走 AI（默认关，即使配了供应商也需手动打开）",
    "ai.allow_routing.label": "允许自动路由",
    "ai.allow_routing.hint": "启用 AI 但暂不接管路由时可关此项，仅供后续手动触发用",
    "ai.providers.section": "供应商 · 密钥独立存于系统凭据管理器",
    "ai.providers.empty": "暂未配置任何供应商",
    "ai.providers.add": "＋ 添加供应商",
    "ai.provider.configured": "已配置",
    "ai.provider.not_configured": "未配置密钥",
    "ai.provider.delete": "删除",
    "ai.provider.delete.confirm": "删除此供应商？其 API Key 将从系统凭据管理器同时移除。",
    "ai.provider.edit": "编辑",
    "ai.provider.referenced": "此供应商被以下档位引用：{tiers}，删除后档位将回退",
    "ai.tiers.section": "档位指派 · 未指派档位自动降级到主档",
    "ai.tier.router": "路由档",
    "ai.tier.router.hint": "意图分类 + 参数抽取，快、便宜、高频调",
    "ai.tier.light": "轻量档",
    "ai.tier.light.hint": "日常单轮任务，中等",
    "ai.tier.main": "主档",
    "ai.tier.main.hint": "多步推理，最强、最贵",
    "ai.tier.unassigned": "未指派",
    "ai.tier.degrade_to": "→ 将降级到「{tier}」",
    "ai.tier.no_provider": "→ 主档未配置，AI 意图辅助不生效",
    "ai.filter.section": "未命中过滤 · 什么样的输入才走 AI",
    "ai.filter.min_query_len": "最短长度",
    "ai.filter.min_query_len.hint": "少于此字符数不走 AI（默认 4）",
    "ai.filter.require_whitespace": "必须包含空格",
    "ai.filter.require_whitespace.hint": "避免\"打错一个字\"就打 LLM",
    "ai.filter.exclude_pure_numeric": "排除纯数字",
    "ai.filter.exclude_pure_numeric.hint": "纯数字/纯符号不走 AI",
    "ai.filter.respect_awareness_url_path": "剪贴板 URL/路径不走 AI",
    "ai.filter.respect_awareness_url_path.hint": "Awareness 已判定为 URL 或文件路径时直接 fallback",
    "ai.advanced.section": "高级",
    "ai.advanced.direct_safe": "允许直接执行 Safe 动作",
    "ai.advanced.direct_safe.hint": "默认关；开启后 AI 高置信的 Safe 动作可跳过 Tab 确认。Dangerous 动作永远需要确认",
    "ai.advanced.timeout": "硬超时（毫秒）",
    "ai.advanced.timeout.hint": "单次 AI 调用最长等待时间。超过自动 fallback。范围 500-30000，慢模型/长回答可拉高",
    "ai.modal.title": "添加 AI 供应商",
    "ai.modal.title.edit": "编辑 AI 供应商",
    "ai.modal.preset": "快速选择",
    "ai.modal.preset.openai": "OpenAI 官方",
    "ai.modal.preset.deepseek": "DeepSeek 官方",
    "ai.modal.preset.siliconflow": "硅基流动",
    "ai.modal.preset.moonshot": "Moonshot（Kimi）",
    "ai.modal.preset.groq": "Groq",
    "ai.modal.preset.openrouter": "OpenRouter",
    "ai.modal.preset.anthropic": "Anthropic Claude",
    "ai.modal.preset.gemini": "Google Gemini",
    "ai.modal.preset.custom": "自定义",
    "ai.modal.kind": "协议",
    "ai.modal.kind.openai_compatible": "OpenAI Compatible",
    "ai.modal.kind.anthropic_messages": "Anthropic Messages",
    "ai.modal.kind.gemini_generate_content": "Gemini GenerateContent",
    "ai.modal.display_name": "显示名",
    "ai.modal.display_name.ph": "如：我的 OpenAI 备用号",
    "ai.modal.base_url": "Base URL",
    "ai.modal.base_url.ph": "留空使用供应商默认",
    "ai.modal.api_key": "API Key",
    "ai.modal.api_key.ph": "sk-... 保存后仅显示 ••••{last4}",
    "ai.modal.api_key.hint": "密钥立即写入 Windows Credential Manager，SQLite 永不存原文",
    "ai.modal.api_key.hint.edit": "留空则保留原密钥；填新值将覆盖旧密钥。",
    "ai.modal.api_key.ph.edit": "留空 = 保留原密钥",
    "ai.modal.models": "模型（每行一个 model id）",
    "ai.modal.models.ph": "gpt-5-nano\ngpt-5-mini",
    "ai.modal.cancel": "取消",
    "ai.modal.save": "保存",
    "ai.modal.save.empty_display": "请填写显示名",
    "ai.modal.save.empty_key": "请填写 API Key",
    "ai.modal.save.empty_base_url": "OpenAI Compatible 协议必须填 Base URL",
    "ai.modal.save.empty_models": "请至少填写一个 model id",
    "ai.saved.toast": "供应商已保存。启用 AI 意图辅助？",
    "ai.saved.enable": "启用",
    "ai.saved.later": "稍后",
    "ai.error.save_failed": "保存失败：{err}",
    "context.add_app": "＋ 添加应用",
    "context.empty": "暂无敏感应用",
    "context.remove.title": "移除",
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
    // 键位用 {{key:X}} 占位符，由 statusbar 走 renderHint 替换成 <kbd> DOM；
    // 未走 renderHint 的调用点（若有）会看到字面 `{{key:X}}`，保证不会被静默截断。
    "hint.open": "打开",
    "hint.copy": "复制结果",
    "hint.fallback": "执行",
    "hint.enter": "{{key:Enter}} {label}",
    "hint.navigate": "{{key:ArrowUp}}{{key:ArrowDown}} 选择",
    "hint.alt_number": "{{key:Alt}}+数字 快捷触发",
    "statusbar.paging": "{{key:PageUp}}{{key:PageDown}} 翻页 · {page}/{pageCount}",
    // 0.8.1 Autosuggestion：statusbar 里的键帽提示（{target} 是补全目标 keyword）
    // 占位符 {key} 由 statusbar 传入具体键帽 Element（Tab 或 ArrowRight，视用户配置）
    "statusbar.autosuggest_accept": "按 {key} 接受补全 → {target}",
    "statusbar.autosuggest_enter": "按 {key} 进入参数模式",
    // 0.8.3 §4.9：Context Suggestion 来源提示,追加在 accept 文案之后（· 分隔）
    "suggestion.origin.selection": "来自划词",
    "suggestion.origin.clipboard": "来自剪贴板",

    // ── 主窗口：搜索框 ──
    // 0.8.5 §6.4：placeholder 移除,改由 Chord 提示（Alt 按下时 `.ghost-chord`）承担引导。

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
    "tab.chord": "Chord",
    "tab.storage": "Storage",
    "tab.debug": "Debug",
    "tab.about": "About",

    // ── Panel titles (h2) ──
    "panel.general": "General",
    "panel.hotkey": "Hotkey",
    "panel.plugins": "Plugin Manager",
    "panel.network": "Network Settings",
    "panel.context": "Context Awareness",
    "panel.chord": "Chord",
    "panel.storage": "Storage",
    "panel.debug": "Debug",
    "panel.about": "About",

    // ── Panel lede (one-line intro under each tab h2) ──
    "general.lede": "Global preferences: theme, language, history, and result limits.",
    "hotkey.lede": "The key that summons Blink and watchdog timing thresholds.",
    "engines.lede": "Apps / files / calculator / built-in actions — where search results come from.",
    "plugins.lede": "Installed third-party and built-in plugins — toggle and configure each.",
    "network.lede": "HTTP / HTTPS proxy shared by the app and all plugins.",
    "storage.lede": "Storage location and cleanup for the database and history.",
    "debug.lede": "Log level and performance samples — leave closed unless troubleshooting.",
    "about.lede": "Version, tech stack, and source repository.",

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
    "general.autosuggest.title": "Input completion",
    "general.autosuggest.enabled.label": "Enable ghost text completion",
    "general.autosuggest.enabled.hint": "Show gray inline suggestion on pinyin-initials hits; press Tab to accept",
    "general.autosuggest.min_score.label": "Fuzzy threshold",
    "general.autosuggest.min_score.hint": "Minimum similarity for partial-pinyin ghost text (0.5~0.95)",
    "general.autosuggest.tab_key.label": "Accept key",
    "general.autosuggest.tab_key.hint": "Replace input with the canonical form and re-run search",

    // ── Hotkey tab ──
    "hotkey.label": "Summon hotkey",
    "hotkey.record.title": "Click then press keys",
    "hotkey.reset": "Reset to default",
    "hotkey.reset.title": "Reset to default",
    "hotkey.tap.label": "Tap threshold",
    "hotkey.tap.title": "Max press-to-release duration before it's treated as a hold (system modifier) instead of a tap. If the key stays down longer than this, it's treated as a hold (preserving system modifier behavior) and Blink is NOT invoked.",
    "hotkey.tap.hint": "",
    "hotkey.grace.label": "Watchdog grace period",
    "hotkey.grace.title": "Focus-loss protection window after the window is shown — prevents hiding before focus transfer completes. Focus-loss detection is suppressed for this duration after the window appears, so it isn't hidden before focus finishes transferring.",
    "hotkey.grace.hint": "",
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
    "debug.section.log": "Logging · Troubleshooting",
    "debug.section.perf": "Performance · Recent samples",
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
    "storage.action.label": "Actions",
    "storage.clear": "Clear history",
    "storage.clear.confirm": "Clear all history?",
    "storage.history_count": "{count} records",
    "storage.loading": "Loading…",

    // ── About tab ──
    "about.version.label": "Version",
    "about.stack.label": "Tech stack",
    "about.license.label": "License",
    "about.repository.label": "Repository",
    "about.update.check": "Check for updates",
    "about.update.checking": "Checking…",
    "about.update.available": "New version v{version} available",
    "about.update.download": "Download",
    "about.update.latest": "Already up to date",
    "about.update.failed": "Check failed, please try later",

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
    "context.desc": "Master switch: turn off to stop all capture instantly",
    "context.lede": "Blink watches what you select / copy / paste, and proactively recommends what to do with it.",
    "context.section.capture": "Capture · Where data comes from",
    "context.section.filter": "Filter · Which apps are skipped",
    "context.section.trigger": "Trigger · What counts as a hit",
    "context.section.present": "Present · How the user sees it",
    "context.filter.title": "Sensitive apps",
    "context.filter.desc": "Pause all capture when these apps are foreground — avoids reading passwords / banking UIs",
    "context.trigger.card.title": "Ghost trigger rules",
    "context.trigger.card.desc": "Suggestions triggered by environment (select English→translate, clipboard URL→open link, etc.); disabling hides the Ghost",
    "context.autosuggest.title": "Input Autosuggestion · Ghost Text",
    "context.bindings.title": "Context Triggers",
    "context.bindings.empty": "No registered Context triggers",
    "context.trigger.text_is_non_target_lang": "Text is non-target language",
    "context.trigger.clipboard_is_url": "Clipboard is URL",
    "context.trigger.clipboard_is_file_path": "Clipboard is file path",
    "context.trigger.selection_non_empty": "Selection non-empty",
    "context.clipboard": "Capture clipboard text",
    "context.selection": "Selection awareness",
    "context.selection.hint": "Auto-captures text you highlight with the mouse and passes it as context when you summon Blink. Limitations: Windows only; requires apps to support UIA TextPattern — browsers (Chrome/Edge/Firefox), Office, VS Code and native Win32 work well; some Electron apps (newer QQ/WeChat/Discord), terminals and games may fail. If no selection is grabbed, just copy the text — Blink will read your clipboard.",

    // ── Chord panel (0.8.5.1 §6.6) ──
    "chord.lede": "When the main window is visible and no input has started, hold Alt to trigger quick actions (Screenshot / Selection Translate / Clipboard).",
    "chord.general.title": "General",
    "chord.enabled.label": "Enable Chord",
    "chord.enabled.hint": "When off, Alt+letter no longer triggers Chord actions",
    "chord.hint_visible.label": "Show hint bar",
    "chord.hint_visible.hint": "Whether the single-line action hint appears in the input box while Alt is held",
    "chord.actions.title": "Actions",
    "chord.actions.hint": "Unchecked actions no longer appear in the hint bar; Alt+letter is also inert",
    "chord.actions.empty": "No Chord actions registered",
    "chord.section.actions": "Actions · Alt + Letter direct",
    "chord.action.screenshot.subtitle": "Drag-select a region and copy to clipboard",
    "chord.action.selection.subtitle": "Grab the currently highlighted text into Blink",
    "chord.action.clipboard_history.subtitle": "Open the clipboard history recall panel",
    "chord.clipboard.title": "Clipboard History",
    "chord.clipboard.enabled.label": "Listen for clipboard writes",
    "chord.clipboard.enabled.hint": "When on, copied content is automatically recorded; type \"clip\" or press Alt+C to recall (if disabled at startup, first re-enable needs one restart to build the listener)",

    // ── AI panel (0.9.1 Phase 6) ──
    "tab.ai": "AI",
    "panel.ai": "AI Intent Routing",
    "ai.lede": "When typing hits no rule, AI tries to understand and suggest actions. Keys are stored in Windows Credential Manager; SQLite only keeps a reference.",
    "ai.enabled.label": "Enable AI intent routing",
    "ai.enabled.hint": "Off by default even after configuring a provider — you must explicitly opt in.",
    "ai.allow_routing.label": "Allow automatic routing",
    "ai.allow_routing.hint": "Turn off to keep AI configured but only trigger manually.",
    "ai.providers.section": "Providers · Keys are stored separately in the system credential manager",
    "ai.providers.empty": "No providers configured yet",
    "ai.providers.add": "+ Add provider",
    "ai.provider.configured": "Configured",
    "ai.provider.not_configured": "No key",
    "ai.provider.delete": "Delete",
    "ai.provider.delete.confirm": "Delete this provider? Its API key will also be removed from the system credential manager.",
    "ai.provider.edit": "Edit",
    "ai.provider.referenced": "This provider is referenced by tiers: {tiers}. Deleting will roll back those tiers.",
    "ai.tiers.section": "Tier assignments · Unassigned tiers degrade to Main",
    "ai.tier.router": "Router",
    "ai.tier.router.hint": "Intent classification + argument extraction; fast, cheap, called often",
    "ai.tier.light": "Light",
    "ai.tier.light.hint": "Everyday single-turn tasks",
    "ai.tier.main": "Main",
    "ai.tier.main.hint": "Multi-step reasoning; strongest and most expensive",
    "ai.tier.unassigned": "Unassigned",
    "ai.tier.degrade_to": "→ Will degrade to \"{tier}\"",
    "ai.tier.no_provider": "→ Main tier unconfigured; AI intent routing is inactive",
    "ai.filter.section": "Miss filter · What input actually reaches AI",
    "ai.filter.min_query_len": "Min length",
    "ai.filter.min_query_len.hint": "Shorter queries skip AI (default 4)",
    "ai.filter.require_whitespace": "Require whitespace",
    "ai.filter.require_whitespace.hint": "Avoid hitting LLM on typos",
    "ai.filter.exclude_pure_numeric": "Exclude pure numerics",
    "ai.filter.exclude_pure_numeric.hint": "Pure digits/symbols skip AI",
    "ai.filter.respect_awareness_url_path": "URL/paths in clipboard skip AI",
    "ai.filter.respect_awareness_url_path.hint": "When Awareness already detects a URL or file path, fall back directly",
    "ai.advanced.section": "Advanced",
    "ai.advanced.direct_safe": "Direct-execute Safe actions",
    "ai.advanced.direct_safe.hint": "Off by default; when on, high-confidence Safe actions may skip Tab confirmation. Dangerous actions still always require confirmation.",
    "ai.advanced.timeout": "Hard timeout (ms)",
    "ai.advanced.timeout.hint": "Max wait for one AI call. Exceeds → fallback. Range 500-30000; raise for slow models / long answers",
    "ai.modal.title": "Add AI Provider",
    "ai.modal.title.edit": "Edit AI Provider",
    "ai.modal.preset": "Quick pick",
    "ai.modal.preset.openai": "OpenAI Official",
    "ai.modal.preset.deepseek": "DeepSeek Official",
    "ai.modal.preset.siliconflow": "SiliconFlow",
    "ai.modal.preset.moonshot": "Moonshot (Kimi)",
    "ai.modal.preset.groq": "Groq",
    "ai.modal.preset.openrouter": "OpenRouter",
    "ai.modal.preset.anthropic": "Anthropic Claude",
    "ai.modal.preset.gemini": "Google Gemini",
    "ai.modal.preset.custom": "Custom",
    "ai.modal.kind": "Protocol",
    "ai.modal.kind.openai_compatible": "OpenAI Compatible",
    "ai.modal.kind.anthropic_messages": "Anthropic Messages",
    "ai.modal.kind.gemini_generate_content": "Gemini GenerateContent",
    "ai.modal.display_name": "Display name",
    "ai.modal.display_name.ph": "e.g. My OpenAI backup",
    "ai.modal.base_url": "Base URL",
    "ai.modal.base_url.ph": "Leave empty to use provider default",
    "ai.modal.api_key": "API Key",
    "ai.modal.api_key.ph": "sk-... Only ••••{last4} shown after save",
    "ai.modal.api_key.hint": "The key is written to Windows Credential Manager immediately; SQLite never stores the raw value.",
    "ai.modal.api_key.hint.edit": "Leave empty to keep the current key; entering a new value overwrites it.",
    "ai.modal.api_key.ph.edit": "Leave empty = keep current key",
    "ai.modal.models": "Models (one model id per line)",
    "ai.modal.models.ph": "gpt-5-nano\ngpt-5-mini",
    "ai.modal.cancel": "Cancel",
    "ai.modal.save": "Save",
    "ai.modal.save.empty_display": "Display name required",
    "ai.modal.save.empty_key": "API Key required",
    "ai.modal.save.empty_base_url": "OpenAI Compatible protocol requires a Base URL",
    "ai.modal.save.empty_models": "At least one model id required",
    "ai.saved.toast": "Provider saved. Enable AI intent routing?",
    "ai.saved.enable": "Enable",
    "ai.saved.later": "Later",
    "ai.error.save_failed": "Save failed: {err}",
    "context.add_app": "+ Add app",
    "context.empty": "No sensitive apps",
    "context.remove.title": "Remove",
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
    "hint.enter": "{{key:Enter}} {label}",
    "hint.navigate": "{{key:ArrowUp}}{{key:ArrowDown}} Select",
    "hint.alt_number": "{{key:Alt}}+number quick launch",
    "statusbar.paging": "{{key:PageUp}}{{key:PageDown}} page · {page}/{pageCount}",
    // 0.8.1 Autosuggestion（{target} = completion target keyword;
    // {key} = kbd Element for user-configured accept key, injected by statusbar）
    "statusbar.autosuggest_accept": "Press {key} to accept → {target}",
    "statusbar.autosuggest_enter": "Press {key} to enter parameters",
    // 0.8.3 §4.9: Context Suggestion origin hint, appended after accept text (· separator)
    "suggestion.origin.selection": "from selection",
    "suggestion.origin.clipboard": "from clipboard",

    // ── Main window: search box ──
    // 0.8.5 §6.4: placeholder removed, chord hint (`.ghost-chord` on Alt) takes over.

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
      if (prop === "textContent" && el.childElementCount > 0) {
        // 保留子元素（如 field-hint-icon），只更新第一个文本节点
        const text = t(el.getAttribute(attr));
        let textNode = [...el.childNodes].find((n) => n.nodeType === Node.TEXT_NODE);
        if (textNode) {
          textNode.textContent = text;
        } else {
          el.insertBefore(document.createTextNode(text), el.firstChild);
        }
      } else {
        el[prop] = t(el.getAttribute(attr));
      }
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
