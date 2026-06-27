//! 入口：wire-up 各模块。每个模块自带 init()，这里只负责装配。

import * as search from "./search.js";
import * as keyboard from "./keyboard.js";
import * as lifecycle from "./lifecycle.js";
import * as contextmenu from "./contextmenu.js";
import { applyThemeFromConfig } from "./theme.js";
import { applyI18nFromConfig } from "./i18n.js";

// 启动应用主题（设置页改 theme 后，lifecycle 在 shown 时重新读取刷新）
applyThemeFromConfig();
// 启动界面语言（静态文本如搜索框 placeholder；shown 时刷新）
applyI18nFromConfig();
search.init();
keyboard.init();
lifecycle.init();
contextmenu.init();
