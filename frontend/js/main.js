//! 入口：wire-up 各模块。每个模块自带 init()，这里只负责装配。

import * as search from "./search.js";
import * as keyboard from "./keyboard.js";
import * as lifecycle from "./lifecycle.js";

search.init();
keyboard.init();
lifecycle.init();
