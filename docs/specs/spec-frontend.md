# 前端实现规范

> **怎么做（HOW）**——前端代码的硬约束 / 铁则。写或改前端代码前先读本文。
>
> 信念层（为什么可辨识度优先、为什么最小操作路径）见 `../product.md §五`；本卷只留落地铁则。各版本落地细节见 `../phases/`。

---

## 第一层：架构与资产组织

### 1.1 无 bundler 铁则（强制）

> **绝对约束**。Blink 前端是纯静态 HTML/CSS/JS，**无 npm、无 bundler、无构建步骤**。

- **禁止**引入需要 build step 的方案（webpack/vite/Tailwind CLI/DaisyUI）
- **允许**：纯 CSS 变量方案（Open Props）、独立 SVG 文件、标准 ES module
- **理由**：保护冷启动 SLO；前端薄到只剩渲染层，业务逻辑全在 Rust
- **反例**：Tailwind 要加构建或撞冷启动 SLO；DaisyUI 同理（见 phases/0.9 §五 关键决策）

### 1.2 CSS 七层架构（强制）

**加载顺序铁则**：`reset → tokens → themes → base → components → views → entries`

| 层 | 目录/文件 | 职责 |
|---|---|---|
| `vendor/` | `open-props/` | Open Props 原始变量（**仅作数值源，非命名源**） |
| `tokens/` | 语义映射 | 组件只见 `var(--space-md)`，**不见** OP 原始 `--size-3`；去 OP 化成本为零（改 `tokens/` 映射即可） |
| `themes/` | `dark.css` / `light.css` / 各主题 | 主题色变量覆盖 |
| `base/` | reset / 基础元素 | |
| `components/` | 14 个跨窗口复用组件 | `.btn` / `.modal-search` / `.icon` 等跨窗口共用 |
| `views/` | 窗口级样式 | `chat.css` / `settings-*.css` 等按窗口拆 |
| `entries/` | 每个 HTML 一个 link | `main.css` / `settings.css` / `chord-screenshot.css`——每 HTML 单 link 入口 |

> 落地决策与 JS 拆分（settings.js 4169→2133 行）见 phases/0.9 §五。

### 1.3 Open Props = 数值源非命名源（强制）

- 业务代码只引用 `tokens/` 的语义变量，**禁止**直接引用 `--size-3` / `--blue-500` 这类 OP 原始命名
- 这样去掉 OP 只需改 `tokens/` 映射，业务代码零改动

### 1.4 窗口预热复用（强制）

> **铁则**：所有窗口在启动后异步预创建并隐藏（`visible:false`），首次 show 直接复用预创建的 webview，不按需创建。

- **预热范围**：`chord-screenshot` / `context-menu` / `voice-overlay` / `chord-pin` / `chat` / `settings` / `content-editor` / `sticky-manager` — 8 个静态 label 窗口全部预热
- **例外**：动态数量的窗口（`sticky-{id}`）按需创建，但窗口外壳复用机制已有（prevent_close + hide）
- **时序**：预热在启动 3s 后异步执行（`preheat_secondary_windows`），不阻塞 Alt+Space 主链路
- **容错**：预热失败打 `warn!` 日志但不阻断，show 函数的 fallback 创建逻辑兜底（`get_webview_window` 不存在则 build）
- **预热窗口的事件注册**：需在预热时注册 `on_window_event`（如 sticky-manager 的 prevent_close + hide），因为 show 函数的复用路径（`is_new=false`）不注册事件
- **settings 预热特殊处理**：预热时补 `strip_window_border` + `enable_rounded_corners`，因为 `open_settings` 的复用路径不调这两个（只在首次创建路径调）
- **内存预算**：8 个预热窗口 + 动态便签，常驻内存 < 300MB（WebView2 每窗口 ~10-20MB）

### 1.5 vendor 第三方库版本管理（强制）

> **铁则**：`frontend/vendor/` 下所有第三方库必须有可追溯的版本记录，不能只放文件不记版本。

- **版本清单**：`frontend/vendor/VERSIONS.md` 是 vendor 目录所有第三方库的版本真源。升级时必更新。格式：

  ```markdown
  | 文件 | 包名 | 版本 | 来源 | 打包方式 | 引入日期 | 引入 phase |
  |---|---|---|---|---|---|---|
  | cherry-markdown.stream.min.js | cherry-markdown | 0.11.9 | npm tgz | 官方 UMD 产物 | 0.17.6 | 0.17 §3.4 |
  | tiptap.bundle.min.js | @tiptap/core+pm+starter-kit+markdown | 3.29.2 | npm + esbuild | 自打包 IIFE | 0.18.3 | 0.18 §3.2 |
  ```

- **自打包产物**（如 Tiptap）：打包脚本归入 `xtask/scripts/`（如 `bundle-tiptap.js`），版本号硬编码为常量（如 `TIPTAP_VERSION = "3.29.2"`），`cargo xtask <name>` 执行。与 `cargo xtask icons`（Lucide 图标脚本生成 sprite）同性质--预处理产物，运行时零构建，不违反无 bundler 铁则。
- **产物头水印**：自打包脚本在产物头部注入版本注释（包名+版本+构建日期），便于二进制文件也能追溯版本。
- **官方 UMD 产物**（如 Cherry）：文件名可不含版本号（保持 `cherry-markdown.stream.min.js`），但 VERSIONS.md 必须记录。
- **升级流程**：① 改打包脚本的版本常量 / 下载新版官方产物 -> ② 重新生成 -> ③ 更新 VERSIONS.md -> ④ review diff（changelog 对比）

---

## 第二层：设计 token 与主题

> **唯一真源**：`frontend/css/theme.css` 的 `:root` 区块（与各主题变量并列）。

### 2.1 分层心智

- **色彩层**（随主题切换）—— `--bg / --text / --accent / --surface / --shadow` 等，`dark|light|gruvbox|nvchad-*|genshin-*` 主题各自覆盖
- **形状与节奏层**（跨主题共享，不随主题变）—— `--radius-* / --transition-*`，固化「同一款 UI 在不同主题下形状一致，只换颜色」

### 2.2 圆角四档（强制）

| Token | 值 | 使用场景 |
|---|---|---|
| `--radius-sm` | 4px | tag/input 内嵌打勾等细节元素 |
| `--radius-md` | 6px | 按钮、卡片主流（15+ 处占比最高） |
| `--radius-lg` | 8px | 大面板/perf-section 类大卡片 |
| `--radius-xl` | 12px | 顶级容器（主窗输入框、模态外壳） |

### 2.3 过渡时长三档（强制）

| Token | 值 | 使用场景 |
|---|---|---|
| `--transition-fast` | 0.1s | 悬停/点击的即时反馈 |
| `--transition-base` | 0.15s | 状态切换默认（color/border 联动） |
| `--transition-slow` | 0.2s | 大幅度形变、淡入淡出、开关滑块 |

### 2.4 token 使用铁则

1. **新增 CSS 优先用 token，不 hardcode 数值**——出现新圆角/过渡值，先反思能否复用现有档；确实特殊（如 chord-ball 圆球 `border-radius: 50%`、结果 hover 超快 `0.08s`）才保留原写法
2. **低频/特例值保留原写法，不搞过度归一**——`border-radius: 10px / 9px / 5px / 2px / 24px` 这类**单次使用**的值继续 hardcode；强行加 `--radius-2xl` 反而稀释语义
3. **色彩绝不 hardcode**——任何颜色必须走主题变量（`var(--text)` 等），否则主题切换会瞎眼；圆角/过渡允许 hardcode 特例，颜色不允许

> 未迁移 token 的历史存量（间距 `--space-*`、阴影 `--elevation-*`）属工程债，按需收敛，不阻塞本铁则。

### 2.5 主题系统

- **选型**：Catppuccin（dark=Mocha / light=Latte）+ auto 跟随系统
- **配色全走 CSS 变量**，主题切换只改变量值
- 当前 82+ 套主题全保留（phases/0.9 §五）

---

## 第三层：视觉铁则（可辨识度）

> 信念层"UI 是功能面不是装饰面"见 `../product.md §五`。本节是它的落地铁则。

### 3.1 中文不用斜体（强制）

**规则**：UI 中的**中文文本**一律不用 `font-style: italic`。英文/代码/引用可保留斜体。

**替代方案**：

| 需求 | 做法 |
|---|---|
| 视觉弱化（次要文本/hint/subtitle） | `opacity: 0.6` |
| 强调 | `font-weight: 600` + 主题强调色 |
| 引用/代码 | `<code>` + 等宽字体 + 弱背景色 |

### 3.2 主题对比度（强制）

- 正文对比度 ≥ 4.5:1，大字 ≥ 3:1（WCAG AA 底线）
- 次要文本弱化只允许两条路径：① `opacity: 0.55~0.75`（保色相）② 主题预设的 `overlay0/1/2` / `subtext0/1`
- **禁止**新调低饱和度灰（如 `#888`）——不同主题下对比度不可控

### 3.3 稳定视觉重量（强制）

- 弱信号绝不撑布局：Ghost 行内叠加、Chord 提示条底部固定
- 首屏结果条数由屏幕高度决定（下限 4 上限 9），不随内容跳变
- 感知/推荐/AI 通道不加高首屏：Context 命中进首屏时替换低分候选，不追加到顶部

### 3.4 分数 DPI 下的透明背景 tile seam（强制）

> **跨版本重复踩坑**：0.12.8 对话窗口踩过，0.17.7 便签管理界面又踩——分数缩放（125%/150%）下透明背景窗口的 GPU 光栅化 tile 边界会产生斜线伪影，肉眼看着像 italic 但根因不是 `font-style`。

- **次级窗口 `background_color` 必须设不透明色**（`windows.rs` 的 `create_*_window` 已统一设置；新建窗口勿漏）
- **避免 `opacity:0` + `transform` 创建合成层**做显隐——用 `visibility:hidden` 或 `.hidden` class 替代
- **中文文本容器补 `font-style: normal` 作防御**——即便没显式写 italic，防止继承链意外触发
- 若怀疑出现斜线，先查窗口透明背景 + DPI 缩放，**别先归因为 italic**

---

## 第四层：组件铁则

### 4.1 键盘提示样式统一（强制）

**唯一真源**：`frontend/css/kbd.css` + `frontend/js/shared/kbd.js`。修改键位视觉只改这两个文件，业务模块不允许再造轮子。

| 场景 | 生成路径 | 反面 |
|---|---|---|
| 单键 | `<kbd class="kbd">Enter</kbd>` 或 `renderKey("Enter")` | 直接写 `[Enter]` |
| 组合键 | `renderCombo("Alt+A")` → 自动嵌 `.kbd-plus` `+` 连接符 | `<kbd>Alt</kbd><kbd>A</kbd>` 无连接符 |
| 平台差异符号（`⌘`/`⌥`） | **不用**——坚持 `Alt`/`Ctrl` 文本 | `⌥A` / `⌘K` macOS 符号 |
| i18n 文案插键位 | `{{key:Tab}}` 模板 + `renderHint` 替换 | 硬编码字符串拼装 |

**分隔符语义**：键与键之间（同组合）用 `+`（`kbd-plus`）；提示之间（不同操作/Chord）用竖线 `│`（`chord-sep`）；`·` 圆点保留给"参数分隔"语义。

### 4.2 图标用包禁 emoji（强制）

**铁则**：UI 占位一律走 **Lucide sprite**，禁用 emoji 字符。这条是防回退铁则——图标包已落地，但需文档钉死防止新代码重新引入 emoji。

**API**（`frontend/js/shared/icon.js`）：
- `renderIcon(name, opts)` / `iconHTML(name)` / `ensureSpriteLoaded()`
- 样式（`frontend/css/components/icon.css`）：`.icon { width/height: 1em; stroke: currentColor; fill: none; stroke-width: 1.75 }`——尺寸跟 `font-size`、颜色跟主题 `currentColor`，零主题适配
- 引用：`<svg class="icon"><use href="#icon-{name}"/></svg>`
- 第三方 PNG 图标走 `.icon-mask`（CSS mask 上色）

**选型理由**：Lucide（`lucide.dev`，ISC 授权）——线性 1.5-2px stroke 与 Blink 极简暗色调一致；每图标独立 SVG 与"无 bundler"铁则契合；`stroke: currentColor` 主题色自动传导；~1600 图标覆盖当前 29 个占位有余量。对比排除 Tabler（偏工程化）/ Phosphor（多权重用不上）/ Remix（偏厚）。详见 phases/0.10 §11.2。

**操作手册**（加/换/升级图标，phases/0.10 §11.6）：

| 操作 | 步骤 |
|---|---|
| 换一个已在 sprite 里的图标 | HTML 改 `<use href="#icon-{name}"/>`；或 JS 改 `iconHTML("xxx")` / `BUILTIN_ACTION_ICONS` map 的 value。可用图标见 `frontend/assets/icons/manifest.json` |
| 新增图标（sprite 里没有） | ① [lucide.dev/icons](https://lucide.dev/icons) 取 kebab-case 名 → ② 追加到 `xtask/scripts/fetch-lucide-icons.py` 的 `ICON_LIST` → ③ 跑 `cargo xtask icons` 重新生成 sprite（本地打包，运行期零网络）→ ④ 引用 |
| 升级 Lucide 版本 | 改 `LUCIDE_VERSION` 常量重跑脚本。注意 1.x 起 tag 无 `v` 前缀；跨大版本可能有图标改名（如 `alert-triangle` → `triangle-alert`） |

**视觉配色**：所有 icon 走 `stroke: currentColor`，父容器 `color: var(--accent)` 让图标跟随主题主色自动变色。尺寸特例（chord-screenshot `.tool-btn .icon` 16px、ai-icon-badge 14px）按需 override，不破坏默认 1em。

---

## 第五层：交互铁则

> 信念层"最小操作路径 / 永远留 escape hatch"见 `../product.md §五`。本节是前端代码里的落地铁则。

### 5.1 异步竞态防护（强制）

**铁则**：异步结果回流前**必校验版本/id 是否过期**，旧结果不得覆盖新结果。这是界面抖动、数据错位的主要根因。

| 场景 | 校验手段 |
|---|---|
| 搜索 `blink://results` | 后端带 query id / 前端比对当前 query，过期丢弃 |
| OCR / 翻译 / 截图识别 | 区域切换或重选时，旧任务回调检查 `cancelled` 标志 |
| 多源并发（sync+async 双 lane） | 每批结果带 epoch，前端只接受 ≥ 当前 epoch 的批次 |

**禁止**：不校验直接 `innerHTML = 新结果`——慢响应覆盖快响应、切换后旧内容闪回。

### 5.2 窗体尺寸稳定（强制）

**铁则**：内容变化不得导致窗口反复 resize 抖动。

- **测高度必须 rAF 后测**：DOM 改完直接读 `offsetHeight` 会拿到旧 layout。参考 `window-size.js::syncWindowSize()`（`requestAnimationFrame` 回调内测量）
- **预留稳定槽位**：频繁变化的内容（如 statusbar 单/双行）用 `min-height` 预占高度
- **首屏结果条数固定**：由屏幕高度算一次，不随内容数量 resize

### 5.3 悬浮层留消失延迟（强制）

**铁则**：悬浮窗 / 浮动面板 / tooltip 关闭前留 **hover 缓冲期**——定时器触发关闭后，鼠标移入即取消关闭，防止鼠标移动路径上误关。

- 实现模式：`mouseleave` 启动关闭 timer → `mouseenter` clear timer。**不裸绑即时隐藏**
- 典型场景：截图工具栏、OCR 面板、钉图窗口、误点保护（点选区外 `setTimeout(…,0)` 延迟绑定关闭监听）
- 参考实现：`frontend/js/screenshot/index.js` 的 `singleClickTimeout` / `mouseleave` 状态清理模式

### 5.4 持久化窗口可靠性（强制）

> 便签的核心承诺是"放在那里就不会丢"--只靠用户点保存或退出时写库，覆盖不了崩溃和系统强制结束。0.16.7/0.16.11 摸出这套范式，未来笔记/Todo 等持久化窗口复用。

- **短防抖自动写库**：内容输入用短防抖落库，不能只靠手动保存
- **失焦 / 隐藏 / 关闭前强制 flush**：防抖中的未保存内容在这些节点必须强制落库
- **区分"用户关闭窗口"和"应用整体退出"**：用进程级 `IS_APP_EXITING: AtomicBool` 标志--退出时不 `prevent_close`、不改 `visible`；退出前 `flush_all_*_windows()` 确保防抖内容落库
- **启动恢复不阻塞主链路**：异步 spawn 读取持久化窗口，不抢 Alt+Space 的焦点；恢复循环加节流/验证
- **显示器变化钳制到可见工作区**：`clamp_*_geometry` 把离屏窗口拉回最近可见工作区
- **单条恢复失败只记日志跳过**：不阻断其他窗口和主窗口 P0 唤起
- **窗口外壳复用**：关闭=hide（`prevent_close` + hide），物理销毁只在删除/退出时；动态数量窗口（如 `sticky-{id}`）按需创建但复用同一套 hide 机制

### 5.5 输入状态：后端单一真源，前端只消费（强制）

> 0.18.7 收敛。此前前端 50ms 轮询 Alt 状态、`set_chord_mode` 反向控制后端、合成 keyup 时间窗猜测，多真源长期分叉导致安装后 Alt/Chord 行为混乱。

**铁则**：物理键状态（Alt 是否按下）、Chord 独占会话、主窗口可见性，**后端是唯一真源**。前端只消费后端推送的状态，不反向控制。

| 禁止 | 替代 |
|---|---|
| 前端定时轮询后端物理键状态做 UI 决策 | 监听后端 `INPUT_STATE_CHANGED` 事件 + 注册时拿快照，用 UI revision 去重/拒绝旧状态 |
| 前端通过 IPC 反向开关后端输入状态（如 `set_chord_mode`） | 后端状态机据 view context（ready/query_empty/ai_mode）自行决定 exclusive |
| 前端用时间窗猜测合成键事件（如 200ms synth keyup） | 后端用 Raw Input + Hook 双源校正，不猜 |

**防御式消费**：键盘事件处理用 `inputState.isAltDown() || e.altKey`--后端快照抵抗 WebView synthetic keyup，事件自带 `altKey` 覆盖状态事件尚未到达的即时边沿（§5.1 异步竞态防护在键盘事件上的具体表现）。不得只用 `e.altKey`。

**视图上下文上报**：前端只上报离散视图状态（`ready/query_empty/ai_mode`），不逐字符同步 query 文本给后端输入状态机；字段未变化不发送。

> 落地（InputUiState 协议、view epoch、register_main_input_view）见 [phases/0.18.7 §3.3/§3.9](../phases/0.18.7-input-state-flow-refactor.md)。

---

## 第六层：工程债（收敛中）

### 6.1 inline style 限制

- **禁止**新增 `style.display = "none"/""` 显隐切换——改 `.hidden`/`.is-open` class + CSS
- HTML 内联 `style="` 尽量外提到 CSS（settings.html / chord-screenshot.html / pin.html 是历史存量，按需清理）

### 6.2 单文件行数阈值

- 目标：单文件不超过 **~1000 行**
- 0.14 已形成的拆分范式（见 `phases/0.14 §九`）：
  - 截图 overlay：`chord-screenshot.js` 作为入口，职责下沉到 `ss-*` 模块
  - AI 设置：`settings/tabs/ai.js` 作为入口，子域下沉到 `settings/tabs/ai/*`
  - 对话样式：`css/entries/chat.css` 作为入口，组件样式下沉到 `css/views/chat/*`
- 入口文件只负责装配、初始化与稳定导出；拆分时必须保留自定义协议、IPC、事件名和加载顺序，不得臆造新的后端 command
- 历史 `style.display` 存量尚未清零；拆分完成不等于样式债清零，后续按触达范围迁移为 class

#### 6.2.1 结构拆分护栏（强制）

> 0.14 §9.2 教训：截图 overlay 拆 `ss-*` 时，一度把取图自定义协议误改成不存在的 `screenshot_get_image` command，静态核对才发现。结构拆分是机械重构，但**外部契约不能动**。

拆分巨石文件（如 `chord-screenshot.js` → `ss-*`、`settings.js` → `settings/tabs/*`、`chat.css` → `views/chat/*`）时，必须逐项保持以下外部契约不变：

1. **Tauri command 契约**：`generate_handler!` 注册的 command 与前端所有 `invoke()` 字面量逐一核对（拆分后 invoke 仍命中后端注册，不能凭空造新 command）
2. **事件名契约**：Rust `blink://*` 事件常量与前端事件常量模块逐一核对（0.14.6 后两端已有集中清单）
3. **加载顺序与导出**：HTML/CSS/JS 入口的加载顺序、模块导出名、初始化时序（拆分后入口装配顺序必须复现原行为）
4. **自定义协议 vs IPC 边界**：如截图像素走 `blink-screenshot` 自定义协议读取，**不能因拆分臆造 `screenshot_get_image` command**——协议与 command 是不同链路

拆分完成后逐项静态核对上述四点，不能只靠"功能正常"冒烟判断。

### 6.3 落地检查清单（每次写前端代码自问）

- [ ] 新写 CSS 里有中文选择器叠 `font-style: italic`？→ ❌
- [ ] 新颜色走了主题变量？硬编码 `#xxx` → ❌
- [ ] 新圆角/过渡用了 token？特例 hardcode 是否真必要？
- [ ] 弱化文本用 `opacity`？新调低饱和度灰 → ❌
- [ ] dark/light 双主题都测过对比度？未测 → ❌
- [ ] 新元素出现/消失时布局有跳变？→ 改行内叠加或固定位置
- [ ] 键位提示走 `kbd.js::renderKey/renderCombo`？自己拼字符串 → ❌
- [ ] UI 占位走 Lucide sprite？塞 emoji → ❌
- [ ] 异步回流校验了版本/id？裸 `innerHTML` → ❌
- [ ] 悬浮层关闭留了 hover 缓冲期？裸即时隐藏 → ❌
- [ ] 新窗口 `background_color` 设了不透明色？透明背景 + 分数 DPI 缩放 → tile seam 斜线（§3.4）
- [ ] 显隐用 `opacity:0 + transform` 创建合成层？→ 改 `visibility:hidden` 或 `.hidden` class（§3.4）
- [ ] 屏蔽了不该出现的原生浏览器右键菜单？需要自定义右键时用 CSS 弹层 + `preventDefault`，不裸留原生菜单
- [ ] 持久化内容失焦/隐藏/关闭前 flush 了？退出与用户关窗口用 `IS_APP_EXITING` 区分了？（§5.4）
- [ ] 新窗口启动恢复逻辑异步化、不抢主窗口焦点？单条失败不阻断？（§5.4）
