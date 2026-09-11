# 0.23.0 编辑器性能 spike 报告（envelope 与 MD/Diff/AI 阈值定案）

> 结论先行：解析成本由**节点数**而非字节数主导（重标记文档 parse 呈超线性，2MB 混合文档需 ~2 分钟，纯段落 2MB 仅 ~150ms）。据此冻结四级门：**可编辑 MD ≤ 32KB；32–128KB 可进但不自动；>128KB 拒绝进 MD；Source envelope 2,000,000 字符**（Source 永不因增强阈值被拒绝编辑/保存）。Diff 安全输入 **≤ 200KB**；AI 整理输入 **≤ 24,000 token**（估算值）。

- 测量环境：Windows 11 (build 26200) / x64 / Node v25.5.0（`--expose-gc`）/ Tiptap 3.29.2
- 复现：`node --expose-gc testdata/editor/markdown/run-perf.mjs`（完整套件约 15 分钟；2MB 混合文档单格即 ~2 分钟）；`node --expose-gc testdata/editor/markdown/run-diff-bench.mjs`（Diff 单项，秒级）
- 局限：Node 只测纯 JS 计算成本。textarea/ProseMirror 的渲染、layout、输入延迟是浏览器侧成本，**由 0.23.5 在真实 WebView2 窗口回归复测**；本报告的 Source envelope 依据内存预算与字符串操作成本冻结。

## 一、MD round-trip（p50/p95，ms）

| 文档类型 | 8KB | 32KB | 128KB | 512KB | 2MB |
|---|---|---|---|---|---|
| prose-cn（纯中文段落）full | 0.7/0.8 | 2.6/3.0 | 8.2/9.9 | 33.8/40.1 | 131/153 |
| markdown-mixed（标题/列表/引用/代码交替）full | 4.8/5.0 | 38.9/41.0 | **502/512** | **7513/7646** | **120017/124614** |
| code-heavy（大代码块）full | 0.2/0.3 | 2.4/2.5 | 32.9/33.7 | 489/499 | 7938/7973 |

- 拆分：瓶颈在 **parse**（mixed 2MB parse p95≈119s，serialize 仅 ~226ms）。serialize 全类型线性、可忽略。
- 超线性根因：mixed 文档节点数随尺寸线性增长，而 parse 成本按节点交互超线性增长（32KB→128KB 尺寸 ×4，耗时 ×13）。升级 Tiptap/marked 后此曲线必须重测。
- 内存（gc 后 heap）：2MB prose 35.1MB / 2MB mixed 73MB / 2MB code 14.9MB——单文档有界，远低于常驻 300MB 预算。

### 冻结：可编辑 MD 尺寸门（含风险门之外的第三道门）

| 区间（UTF-8 字节） | 行为 |
|---|---|
| ≤ 32KB | 正常进 MD（最坏 mixed round-trip p95 ≈ 41ms，无感） |
| 32KB – 128KB | 允许手动进 MD 但提示"较大文档，切换可能卡顿"；`preferred` 来源不自动进 MD |
| > 128KB | 拒绝进入可编辑 MD，保持 Source（mixed 128KB p95≈0.5s 已超交互预算一个量级） |

> 该门与 `MarkdownViewPolicy`（phase §3.3）叠加执行：风险门（内容结构）优先，尺寸门次之，来源偏好最后。

## 二、Diff 阈值（token 级 Myers：CJK 单字 + ASCII 词 + 公共前后缀预剪 + D≤4000 熔断）

| 尺寸 | 编辑率 | tokens | diff p50/p95 (ms) | D | 熔断 |
|---|---|---|---|---|---|
| 16KB | 1% | 1,298 | 0.0/0.3 | 26 | 否 |
| 16KB | 5% | 1,298 | 0.3/0.6 | 108 | 否 |
| 64KB | 5% | 5,138 | 2.9/3.9 | 548 | 否 |
| 200KB | 5% | 15,928 | 32.7/35.2 | 1,786 | 否 |
| 500KB | 1% | 39,558 | 9.7/19.2 | 980 | 否 |
| 500KB | 5% | 39,558 | 178/182 | — | **是**（部分轮次 D>4000） |
| 64KB | 30% 重写 | 5,138 | 97 | 2,725 | 否 |

- token 化成本可忽略（500KB p50≈16ms）。
- **冻结：Diff 安全输入 ≤ 200KB（≈ 20,000 token），5% 编辑率 p95 ≤ 35ms**；超限或 D>4000 熔断 → 降级为"仅复制结果"（复用 phase §3.7 既有出口）。AI 整理输出与输入差异率通常远低于 5%，200KB 内余量充足。

## 三、AI 整理输入门（推导值，非测量值）

推导链（复用现有 `token_budget.rs` 单一真源）：
1. context 下限取 tiered fallback 最小值 **32K**（本地/私有 host：`TIERED_LOCAL_CONTEXT_LIMIT`）
2. 减保留输出 4K、安全余量 ~1.6K（`compute_safety_margin` 5% clamp [256,4096]）→ 可用输入 ≈ 26K token
3. 留 ~8% 估算误差余量（`estimate_text_tokens` ±20% 启发式）→ **冻结：`estimate_text_tokens(整理输入) ≤ 24,000`**
4. `max_tokens = clamp(input_est + 2048, 1024, context_limit − input_est − safety_margin)`（整理输出约等于输入规模）

## 四、Source envelope（冻结）

| 项 | 值 | 依据 |
|---|---|---|
| Source 硬上限 | **2,000,000 字符**（约 6MB UTF-8 CJK），超限拒绝载入并提示分拆 | 2MB EOL 归一化 p95=0.2ms、heap 有界；textarea 打字/滚动体验属浏览器侧成本，**0.23.5 真窗复测后如有退化再下调** |
| 增强降级次序 | Source 始终可编辑保存；MD/Diff/AI 按各自门独立降级 | 验收 6.1"Source 超过增强阈值仍可编辑保存" |

## 五、对后续子版本的输入

| 子版本 | 输入 |
|---|---|
| 0.23.1 | MD 尺寸门并入视图决策；保存路径补 EOF 换行（roundtrip 报告 §二）；round-trip 语料作为风险门回归基准 |
| 0.23.4 | Diff 参考实现（`lib/bench-lib.mjs` 的 `tokenize`/`myersDiffCost`，含 trace 快照回溯的待补项）；200KB/熔断降级门；AI 24K token 门与 max_tokens 公式 |
| 0.23.5 | 真实 WebView2 窗口复测：textarea 大文档打字/滚动、Tiptap 切换实测延迟、长会话内存曲线 |
