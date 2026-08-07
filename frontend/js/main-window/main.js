//! 入口：wire-up 各模块。每个模块自带 init()，这里只负责装配。

import * as search from "./search.js";
import * as keyboard from "./keyboard.js";
import * as lifecycle from "./lifecycle.js";
import * as contextmenu from "./contextmenu.js";
import * as ghost from "./ghost.js";
import * as statusbar from "./statusbar.js";
import * as autosuggestConfig from "./autosuggest-config.js";
import * as chord from "./chord.js";
import * as aiMode from "./ai-mode.js";
import * as cmdMode from "./command-mode.js";
import * as inputState from "./input-state.js";
import { applyThemeFromConfig, applyGlassOpacityFromConfig } from "../shared/theme.js";
import { applyI18nFromConfig } from "../i18n/index.js";
import { ensureSpriteLoaded } from "../shared/icon.js";

// 图标 sprite：早注入，让后续 init() 拼 DOM 时 <use href> 可解析
// （fire-and-forget，失败降级为无图标，不阻塞主流程 —— 见 icon.js catch 分支）
ensureSpriteLoaded();

// 启动应用主题（设置页改 theme 后，lifecycle 在 shown 时重新读取刷新）
applyThemeFromConfig();
applyGlassOpacityFromConfig(); // 启动时应用毛玻璃透明度
// 启动界面语言（静态文本如搜索框 placeholder；shown 时刷新）
applyI18nFromConfig();
ghost.init();
statusbar.init(); // 订阅 ghost 变化——必须在 ghost.init 后
// autosuggest 前端配置（tab_key）—— 异步 fetch，不 await（默认 Tab 已可用，回填后无缝切换）
autosuggestConfig.init();
search.init();
keyboard.init();
lifecycle.init();
contextmenu.init();
chord.init();
aiMode.init(); // 0.17.6: AI 模式初始化（注册 CHAT_STREAM / CHAT_CONFIRM_ACTION 监听）
cmdMode.init(); // 0.18.6: 命令模式初始化（创建 hint DOM）
inputState.init(); // 输入状态桥接初始化（注册 listener + register_main_input_view）
