# 0.23.0 Markdown Round-trip 报告（Tiptap 3.29.2 语料定案）

> 结论先行：**验证子集 14 类样本可进可编辑 MD**（其中 2 类逐字节一致、12 类按既定规范化重写，均无内容丢失、二次解析全部到不动点）；**7 类结构必须拒绝进入可编辑 MD**（HTML 块/内联/注释、脚注、数学、GFM 表格、含反引号的行内代码），全部有实际输出证据。回归 runner 全绿：33/33 样本实际行为与期望分类一致。

---

## 一、方法与版本锁定

| 项 | 值 |
|---|---|
| Tiptap 版本 | **3.29.2**（`@tiptap/core` `@tiptap/pm` `@tiptap/starter-kit` `@tiptap/markdown` `@tiptap/extension-task-list` `@tiptap/extension-task-item` 六包同版，与 `xtask/scripts/bundle-tiptap.js` 锁定一致） |
| 解析引擎 | `@tiptap/markdown` 内置 `MarkdownManager`（marked 内核） |
| Manager 构造 | `indentation {style:"space", size:2}`、`markedOptions:{}`、扩展面 `StarterKit + TaskList + TaskItem` —— 与生产 `Markdown` 扩展 `onBeforeCreate` 的构造逐项一致 |
| 生产链路对应 | `setContent(md, {contentType:"markdown"})` ≡ `manager.parse(md)`；`editor.getMarkdown()` ≡ `manager.serialize(editor.getJSON())` |
| 运行环境 | Windows 11 / Node v25.5.0（headless，无 DOM 依赖） |
| 复现 | `node testdata/editor/markdown/run-roundtrip.mjs`（`--json` 机器可读；依赖见 `lib/engine.mjs` 头注释） |

**一致性声明**：本报告结论对生产编辑器有效的前提是 0.23.1 的 `MarkdownIrEngine` 沿用上述扩展面与 Manager 配置；升级 Tiptap 版本必须重跑本 runner。

## 二、判定规则（0.23.0 冻结）

语料期望分类记录在 `corpus/manifest.json`，runner 负责"实际行为 ≡ 期望"的回归：

| 实际分类 | 判定 | 说明 |
|---|---|---|
| `identical` | 可进可编辑 MD | round-trip 逐字节一致（EOF 换行容差内，见下） |
| `normalized` | 可进可编辑 MD | 字节有差但二次 round-trip 到不动点；重写为既定规范化形式，无内容丢失 |
| `unstable` | 拒绝 | 二次 round-trip 仍变化，解析不稳定 |
| 内容丢失/语义损坏 | 拒绝 | 由期望标注 + 人工核对 rt1 证据定案（下表逐条给出） |

**EOF 换行约定**：`serialize()` 不在文档末尾输出换行。冻结决策——0.23.1 的 MD→文本保存路径在序列化后统一补一个末尾换行；round-trip 的"逐字节一致"按补换行后比较。纯视图切换的逐字符保真与此无关，由会话级原文检查点保证（phase §3.3，未编辑 MD 不走序列化）。

## 三、结果汇总

33 个样本：期望 `identical` 类 14 个全部实测 identical；期望 `normalized` 类 12 个（10 个实测 normalized、2 个实测 identical 的良性落点：`escapes`、`ordered-start`）；期望 `reject` 类 7 个（6 个 normalized、1 个 unstable）。runner 输出 0 失败。

### 3.1 支持类（可进可编辑 MD，identical）

标题（ATX 1-6）、段落（中文/混排/全半角标点）、粗体/斜体/删除线/粗斜体、行内代码、围栏代码块（带语言/无语言）、内联链接（含 title、中文参数 URL）、无序/有序/任务列表（含嵌套）、引用（含嵌套）、分隔线、综合文档、**未识别自定义语法**（`:::容器`、`==高亮==` 作为字面文本逐字节保留）。

### 3.2 规范化类（可进可编辑 MD，重写后稳定）

| 样本 | 规范化行为 |
|---|---|
| `list-star` | `*` 列表标记 → `-` |
| `strong-underscore` | `__粗体__` → `**粗体**` |
| `emphasis-underscore` | `_斜体_` → `*斜体*` |
| `setext-heading` | Setext（下划线式）标题 → ATX `#` |
| `blank-lines` | 多余空行/尾随空格收敛 |
| `list-loose` | 宽松列表（项间空行）→ 紧凑列表 |
| `autolink` | `<url>` → `[url](url)`；邮箱 → `[x](mailto:x)` |
| `entities` | HTML 实体解码为字面字符（`&amp;` → `&`） |
| `hard-break` | 行尾反斜杠换行 → 双空格换行 |
| `tilde-fence` | `~~~` 围栏 → ```` ``` ```` 围栏 |
| `escapes` | 转义序列保持语义（实测逐字节一致） |
| `ordered-start` | 非 1 起始编号**保留**（实测逐字节一致） |

嵌套列表缩进规范：无序嵌套输出 2 空格、有序嵌套输出 3 空格（语料已按此形定稿，作为 0.23.1 保存的规范化基线）。

### 3.3 拒绝类（必须保持 Source，实际输出证据）

| 样本 | 实际行为 | 证据（rt1 关键片段） |
|---|---|---|
| `table`（GFM 表格） | **整块内容丢弃** | 全表消失，仅剩"表格后的普通段落"——最严重的内容丢失 |
| `footnotes` | 语义改写为链接 | `[^1]: 第一条…` → `[^1](第一条脚注内容。)`——脚注引用关系变成链接语法 |
| `math` | 命令被转义污染 | `\int_{-\infty}` → `\int\_\…`（下划线被防斜体转义），LaTeX 失效 |
| `html-block` | 标签转义为字面文本 | `<div class="warning">` → `&lt;div class="warning"&gt;`，块结构丢失 |
| `html-inline` | 同上 | `<b>粗体标签</b>` → `&lt;b&gt;粗体标签&lt;/b&gt;` |
| `html-comment` | 同上 | 注释内容变成转义可见文本（批注语义丢失） |
| `inline-code-backtick` | **unstable** | ``` ``a ` b`` ``` → `` `a ` b` ``（重写错误），二次解析结果再变——Tiptap 3.29.2 序列化器缺陷 |

> HTML 三样本二次解析均稳定（normalized），但渲染语义已不可逆丢失，按"未知或可能丢失的结构拒绝进入可编辑 MD"原则归 reject。

## 四、对 0.23.1 的输入（风险门规则）

1. **进入可编辑 MD 前的预检**（正则级、跳过 code fence/span 后匹配）：
   - GFM 表格：连续两行 `|…|` 且次行为 `|---|` 形态 → 拒绝
   - 脚注：`\[\^[^\]]+\]` → 拒绝
   - 数学：`$$` 块（行内 `$…$` 误伤率高，首版不检，只检 `$$`）→ 拒绝
   - HTML：非代码行的 `<[a-zA-Z][^>]*>` 标签 → 拒绝
   - 行内代码含反引号：解析后 JSON 遍历，`code` 节点 text 含 `` ` `` → 拒绝
2. **规范化告知**：命中 3.2 类重写不阻断，但 MD 首次编辑后保存的状态条应提示"已按编辑器规范重写"。
3. **EOF 换行**：MD 序列化产物保存时补末尾换行（见 §二）。
4. **纯切换保真**：不依赖 round-trip，由未编辑检查点保证——"未编辑 MD 切回原文逐字符不变"对本报告所有样本成立。

## 五、已知局限

- 语料未覆盖 `Underline`（StarterKit 内置但 Markdown 双下划线语法非通用）、图片语法（`![]()`，编辑器场景首版不承诺）。
- `marked` 的 GFM 行为随 Tiptap 版本变化，升级必须重跑 runner（`corpus/manifest.json` 的期望分类即回归基准）。
