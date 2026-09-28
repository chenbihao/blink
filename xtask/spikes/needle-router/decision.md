# Decision — Needle Router Spike（0.24.6）

> 日期：2026-09-28
> 结论：**no-go（当前形态不可 Adopt）**；运行形态与接入路径数据保留，供 §5.7 触发后复用。
> 归因（phase 5+6 定论）：Needle 3 多语言不含中文（中文=分布外）是 natural_zh 全 0 的第一层原因；但中文原生的 qwen3.5-0.8b 犯同款错误——**根因是任务语义错位：LLM 按内容理解分类，Blink 要求按语言同源性等形式特征分类，而后者本就是规则（CJK 检测）的完美任务**。产品语境 prompt 注入无解（两模型 × 三指令语言 × few-shot 全失败），微调是唯一路径，语料来源（0.24.3 观测不记原文）才是 0.25 立项的真正前置。
> 范围：可行性验证，不代表 Needle 3 本身质量的终审——评估的是"off-the-shelf 权重 +
> prompt 配置"能否直接承载 Blink 主窗口路由建议，结论是不行；微调路线未在本 spike 范围内。

## 一、结论速览

| 问题 | 结论 |
|---|---|
| 能否常驻 | **能**。`--serve` 常驻 HTTP 稳定（60 连发 0 错误、20s idle 不死、2 并发 OK），ready 0.51s |
| 常驻 vs 单发 | 常驻 warm p50 114~126ms；单发每请求冷进程 p50 432ms。**差 3~4 倍，常驻是唯一可用人态** |
| Blink 输入分布是否适用 | **不适用（核心否决项）**。48 样本 macro 准确率 0.375/0.396；中文原生对照 qwen3.5-0.8b 也仅 0.562 且 natural_zh 同样 0.0——跨模型定论见 §八 |
| 同步 ≤20ms 预算 | 生成式（Needle/qwen）warm p50 均 ~110ms 不满足；**embedding 路线 p50 15ms 满足**（§九） |
| 什么路线有增量 | **三层混合**（§九）：规则形式层（100%）+ 判别式语义层（natural_zh 0.83、15ms，产品语境可经标签描述注入）+ 生成式整体退出路由 |
| 引擎化接入 | 可行但合同缝隙大（4 项缺失）；推荐 **方案 B**（ModelIntentProducer + infra 原语） |

## 二、运行形态数据（phase 1/2/2b）

| 指标 | 数值 |
|---|---|
| 制品体积 | runner 1.22MB + 权重 33.7MB + tokenizer 0.23MB ≈ **35.1MB** |
| 单发 `--prompt`（冷进程全流程，n=10） | p50 **432ms** / p95 439ms，峰值 RAM 82.6MB |
| 常驻 ready | 0.51s |
| 常驻首请求 vs 后续 | 144ms vs 106ms p50（预热差小，无懒加载大坑） |
| warm·英文长句（n=40） | p50 114ms / p95 133ms |
| warm·短关键词（n=40） | p50 126ms / p95 263ms / max 510ms（**方差大**，且与长句倒挂，非确定性延迟） |
| 常驻 RAM（启动/负载） | 64.5MB 工作集 |
| 稳定性 | 60 连发 0 错误；20s idle 后仍存活且 98ms 响应 |
| `--depth 4`（29M 子网络） | 长句 p50 **529ms**/max 914ms——reasoning 复读退化（`cooking_cooking_...` 跑满 token 上限，自检把调用压进 `suppressed_calls`）。**浅层不可用，full depth 必选** |

## 三、数据集适用性（phase 3，核心否决项）

期望标签按 0.24 规则版语义（英文自然长句→translate 建议面；中文自然语言→ask_ai；
URL→open_url；路径→open_path；应用名/结构化→none；明确 keyword→translate 确定路由）。

| 类别 | minimal | enriched system |
|---|---|---|
| url | 0.50 | **0.00** |
| path | 0.00 | 0.00 |
| english_word | 0.67 | 0.67 |
| long_sentence_en | 0.33 | 0.33 |
| natural_zh | **0.00** | **0.00** |
| keyword | 0.50 | 0.67 |
| structured | 0.67 | 0.33 |
| mixed | 0.33 | 0.50 |
| **总体** | **0.375** | **0.312** |

三个结构性失败模式（都不靠 prompt 可修）：

1. **语义错位**：Needle 的工具语义是"用户意图分类"（英文长句是"对助手说的话"→none），
   Blink 要的是"建议面启发式"（非目标语言自然文本→建议翻译）。`long_sentence_en` 0.33
   的错向几乎全是 →none（conf 0.47~0.75 高置信），这不是噪声是语义分歧。
2. **产品语境注入失败**：中文 query 全部误判 translate（模型不知道"UI 语言=中文，
   中文提问是 ask_ai"）。enriched system prompt 试图教，结果把 URL/路径也教成了
   "外语文本→translate"（url 类 0.50→0.00）。**121M 模型承载不了这种语境配置，
   产品语义只能走微调**（Needle 官方支持 finetune，未在本 spike 范围）。
3. **confidence 分层不成立**：match 组均值 0.352 vs mismatch 组 0.295（enriched
   0.211 vs 0.119），重叠严重；且存在**高置信漏报**（`localhost:3000`→none conf
   0.758、`\\server\share`→none conf 0.685）。"低置信回退规则"策略在完整分布上无效。

**危险误路由**（非执行类输入被判 open_url/open_path）：minimal 2 例（`git`→open_url
conf 0.073、中文句→open_url conf 0.274），enriched 0 例。绝对数小但非零，且 §5.7
要求"危险误路由必须为 0"——在 confidence 分层失效的前提下无法用阈值保证，只能靠
"执行类意图永远由规则短路"兜底。

**keyword 类的启示**：明确 keyword（`翻译`/`fanyi`/`translate`）0.5~0.67——规则 100%
准的确定路由，模型反而不稳。**"确定性规则高于模型"不是保守设计，是数据支持的必然。**

## 四、协议合同缝隙（phase 4，对照 EngineManager）

官方 `--serve` 与 Blink 引擎健康合同的差距（4 项缺失 + 1 项语义差异）：

| EngineManager 合同 | needle --serve 现状 |
|---|---|
| `GET /health` 返回 200 + 身份 JSON | **404，无 health 端点** |
| `X-Engine-Token` 鉴权 | **无鉴权，直接放行** |
| 响应回显 engine_id/instance_id/token_fingerprint | **无任何身份字段** |
| `POST /shutdown`（带 token） | **无 shutdown 端点** |
| 无状态请求 | **turns 累积**，须每请求 `POST /reset`（并发下有竞态风险） |

可取项：支持并发请求（2 在途 OK）；进程级回收用现有 `ManagedProcess` Job Object 即可。

## 五、接入路径评估（回答"引擎页纳入路由引擎 / 对外暴露 / capability 接口"）

**方案 A：进 EngineManager 作首个 HTTP transport 引擎**（引擎页 `local-model-runtime`
自动出卡片）。可行但增量面大：`NeedleAdapter` 五方法 + `CapabilityKind::Routing` 闭合
枚举扩展 + health 合同需 wrapper 补齐 4 项 + 安装事务全套。**在数据适用性 no-go 的前提下
属于过度投资**；且 needle.exe 秒级启动、35MB 制品用全量模型安装事务（下载/校验/切换/
回滚）是重武器打蚊子。

**方案 B（推荐保留）：`ModelIntentProducer` + infra 原语按需取用**——0.24 §3.9 既定
路线。进程管理复用 `ManagedProcess`（Job Object 回收）+ `EndpointAllocator`（端口），
模型资产可复用 `model_storage` slots/active.json 事务，配置挂 `engine:needle` shard
（有 `engine:start_menu`/`engine:calc` 命名先例），不进 `EngineRegistry`、不扩
`CapabilityKind`。设置面如需开关，挂上下文面板 Smart Tab 卡（0.24.5 模式）而非引擎页。

**对外暴露**：若未来想让 needle 服务对第三方可用——capability 体系注册 `route_intent`
能力（`inventory::submit!` 一行 + `CapabilityPolicy` 出口策略）是轻路径，天然获得
MCP/CLI 暴露与审计；**不建议**直接把 needle.exe 端口暴露到引擎服务层（无鉴权无身份，
违反 0.22 引擎信任边界）。

## 六、对 0.24 §5.7 触发决策的影响

- 触发条件（线上观测误判超阈值 / 规则无法分类的样本聚集）**不变且被本 spike 加强**：
  off-the-shelf Needle 不能直接替换规则，未来若触发，前置条件应是**用 0.24.3 观测数据
  微调**后再评估，评估继续以规则版为基线。
- "同步 ≤20ms" 预算口径修正为"异步建议 lane 晚到可接受"（0.24 revision/seq 协议天然
  支持晚到替换）；结果 lane <20ms 预算不受影响的表述不变。
- depth 4 子网络不可用已实测钉死； Needle 2 未测（Needle 3 已是当前官方主推）。

## 七、归因实验（phase 5，20260928 追加）：三个结构性失败是姿势还是能力？

针对"是没放目标语言提示词、还是使用姿势不对"的问题，做了 10 组对照
（工具形状 × system × forced 三个姿势变量，结果见 `results/phase5-posture.json`，
脚本 `phase5_posture.py` 可全量重跑）：

| 实验 | 姿势 | overall | natural_zh | 该姿势修复了什么 |
|---|---|---|---|---|
| P3-minimal（基线） | v1 单工具 enum·意图语义 | 0.375 | 0.0 | — |
| P3-enriched | v1 + 语境教条 system | 0.312 | 0.0 | 无（URL 反被教坏 0.5→0） |
| E1 | v2 单工具 enum·**建议面语义重写** | 0.396 | 0.0 | keyword 0.5→0.83 |
| E2 | v3 **五工具零参**（原生 tool-selection） | 0.375 | 0.0 | **url 0.5→1.0** |
| E3 | v3 + 无 system 语境 | 0.354 | 0.0 | url 保持 1.0 |
| E4 | v2 + **--forced**（消除空调用） | 0.396 | 0.0 | **keyword 1.0、长句 1.0**、empty 17→0 |
| E5 | v4 中文规则一字一句强化 + forced | 0.271（最差） | **0.0** | 无，规则越多越乱 |
| E6 | v3 + forced（两类修复叠加） | 0.333 | 0.0 | url 保 1.0，keyword 回落 0.67 |

**归因结论**：

1. **姿势确实有影响（假设部分成立）**：语义重写、多工具形状、forced 各能撬动一类
   （keyword / url / 长句+空调用），初始姿势不是最优。但修复**互斥不可叠加**
   （E6 证实），没有任何组合整体超过 0.4。
2. **中文语境对 prompt 完全免疫（假设否决）**：目标语言提示词从"没放"（E3）到
   "enriched 教条"（P3）到"一字一句 Rule 1 强制"（E5）共 5 种放法，natural_zh
   十组全 0.0。**第一层原因后经 §八修订：Needle 3 多语言不含中文（分布外输入）；
   但中文原生 qwen3.5-0.8b 对照犯同款错误，故"等中文支持"也非解——根因是任务
   语义错位，见 §八定论。**
3. **forced 的双刃剑本质**：Needle 的 none 语义就是"不调用"（README："returns an
   empty list rather than guessing"）。forced 消除空调用修复 keyword/长句，同时把
   应用名的自然 none 表达堵死（english_word 0.83→0）。**"强制分类"与模型设计哲学相悖。**

**解决方案评估**：

- **prompt 层：无解**（上表穷尽）。
- **微调层：唯一可行路径**。把"UI 语言=中文、中文提问=ask_ai、建议面语义"烧进权重；
  语料可由 0.24.3 观测数据（impression/adopt 日志）构造——与 §5.7 触发条件形成闭环。
  微调后姿势基线建议在 v3 多工具（url 已满分）与 v2+forced（keyword/长句满分）之间
  用验证集选。
- **混合架构（新增可测试假设）**：每类输入的最佳姿势不同且不可叠加，恰好论证 Blink
  既有"规则短路 + 模型补位"架构的正确性——URL/路径/keyword/结构化类规则≈1.0 且模型
  无增量，模型只在规则不确定的**自然语言灰色地带**（长句 vs 应用名歧义、混合意图）补位，
  此时模型只需在 translate/ask_ai/none 三值内决策且输入已被预过滤为自然语言，分布窄
  得多。该假设需 0.24.3 线上观测定义灰色地带分布后另行验证，不在本 spike 范围。

## 八、归因修订与跨模型对照（phase 6，20260928 追加）：任务语义错位定论

**事实修正**：Needle 3 官方多语言支持为英/法/西/德/荷/意/波 7 种语言，**不含中文**
（官方 changelog 原文："Multilingual: Needle 3 now supports English, French, Spanish,
German, Dutch, Italian, Polish, with more languages coming"）。§七"121M 训练分布硬上限"
的第一层归因落实为：**中文是分布外输入**。

**但"等中文支持"救不了——qwen3.5-0.8b 对照实验（phase 6a）**：用 LM Studio 本地
qwen3.5-0.8b（阿里、中文原生）跑同一 48 样本（`phase6a_qwen_local.py`，需 LM Studio
跑于 127.0.0.1:1234，`reasoning_effort=none` 关闭其默认 thinking——否则 0.8B 在此任务
reasoning 失控单请求烧 2000 token / 20s）：

| 姿势 | overall | natural_zh | url | path | 备注 |
|---|---|---|---|---|---|
| Q1 zero-shot（英文规则描述） | **0.562** | **0.0** | 1.0 | 1.0 | 长句问句→ask_ai（按交际意图） |
| Q2 few-shot(3) | 0.521 | 0.0 | 1.0 | 1.0 | 示例反而带偏长句 0.17 |
| Q3 中文指令探测 | — | ≈0（5/6→translate） | — | — | 格式稳定性也下降（输出非标签） |

- qwen 整体 0.562 显著高于 Needle 最高 0.396，**URL/路径双双 1.0**（Needle 路径 0.0）；
  延迟 p50 109ms 与 Needle 114ms **相当**（分类任务输出 token 极短，prefill 主导，
  0.8B 并不比 121M 慢）；代价是制品 ~450MB(q4)/内存 footprint 约 10 倍。
- **决定性发现：中文原生的 0.8B 与无中文的 121M 犯同款错误**——中文句子→translate、
  英文问句→ask_ai（按"这是问句该找助手"的内容理解，而非"语言是否与 UI 相同"的形式
  判定）。三种指令语言 × few-shot × 两个模型，产品语境（UI 语言=中文，中文提问=ask_ai）
  **全部注入失败**。

**定论：任务语义错位，不是语言支持也不是参数量。** Blink 路由要求按**形式特征**
（语言同源性/文本类型：CJK 范围、URL/路径正则、keyword 表）分类；生成式 LLM 无论
0.12B 还是 0.8B、中文原生与否，预训练先验（内容语义理解）持续压过元语言指令。而
0.24 `gating.rs` 的 `needs_translation`/`classify_query`（CJK 检测 + 结构正则）对
natural_zh/url/path/keyword/structured 本来就是 100%——**语言同源性判定是规则的完美
任务，不是模型欠训练，是任务不该交给它**。模型的潜在价值收窄到规则真正不确定的
**内容语义灰色地带**（歧义短语、混合意图、隐含请求）。

**对微调决策的影响**：微调仍是把产品语境烧进权重的唯一路径，但候选应扩为
**双模型微调对照**：Needle（35MB 制品最优，中文分布外需更多语料覆盖）vs
qwen3.5-0.8B（中文原生、延迟相当、制品/内存 10 倍）。微调 spike 的真正问题从
"能不能到 0.9"变为"**语料从哪来**"（0.24.3 观测不记原文，合成语料有循环论证局限）——
这决定 0.25 是否立项，而非模型选型。

## 九、判别式路线实测与决策模型调研（phase 6b/7，20260928 追加）

**embedding 标签相似度（`phase6b_embedding.py`，LM Studio nomic-embed-text-v1.5，768 维，
零训练零下载）**：样本与 5 个标签描述做 cosine 最近邻，两姿势对照：

| 姿势 | overall | natural_zh | path | english_word | structured | 延迟 p50 |
|---|---|---|---|---|---|---|
| A 纯标签名 | 0.292 | 0.17 | 0.17 | 0.0 | 0.0 | 17ms |
| B 标签+描述 | **0.396** | **0.83** | **1.0** | 0.0 | 0.0 | **15ms** |

- **natural_zh 0.83 破局**：LLM 两家（Needle/qwen3.5-0.8B）全灭的类别，判别式表示
  5/6 判对——embedding 语义空间没有生成式 LLM 的「中文=翻译候选」先验，产品语境
  （中文提问=ask_ai）通过标签描述即可注入。
- **唯一进同步 ≤20ms 预算的路线**（p50 15ms）；形式类（应用名/结构化）全 0——
  语义距离对它们无意义，但这正是规则（正则/应用索引）100% 的领地。
- **long_sentence_en 0/6 全错向 ask_ai**：英文自然句与「助手提问」描述语义相似度
  高于「外语文本」描述——与 LLM 教训同构：**语言同源性判定是形式任务，谁都不该
  塞给语义模型**。mixed 翻译意图类 3/6 且 top2 中 translate 恒为第二（margin <0.08）。

**三层混合架构（本 spike 的最终架构产出）**：

1. **形式层（规则，<1ms，100%）**：URL/路径/结构化/keyword/应用名命中/**CJK 语言
   判定**——0.24 `gating.rs` 全部已有，无需任何模型；
2. **语义层（embedding/NLI/决策模型，~15ms，进同步预算）**：仅处理规则不确定的
   自然语言歧义（隐含翻译请求、模糊短语、混合意图）——natural_zh 0.83 证明产品
   语境在此层可通过标签描述注入；
3. （可选）生成层（LLM）：复杂推理，不承担路由。

即：long_sentence_en/natural_zh 的判定根本不需要模型（CJK 检测即可），模型领地收窄
到 mixed 歧义类——这是比 §八"灰色地带"更精确的边界，且 0.8B/121M/生成式整体退出。

**laya 决策模型实测（`phase7_laya.py`，20260928 网络恢复后完成）**：Jev 是 2026 新品类
「决策模型」（state+typed questions→预定义答案上的分布，非生成）；laya 为开源实现
（multilingual = mmBERT-base 322M、647MB、100+ 语言含中文、1024 上下文、请求时定义
标签 choice/score/noul、Apache 2.0；生态 OpenJev/Ollaya）。48 样本实测：

| 指标 | 数值 | 评价 |
|---|---|---|
| overall | **0.417** | 介于 Needle 0.396 与 qwen 0.562 之间，官方 Honest Limits"base 零样本接近随机"应验 |
| natural_zh | 0.17 | 远低于 embedding 0.83——判别式≠自动赢，mmBERT 零样本同样不懂产品语境 |
| structured / path | 0.83 / 0.67 | 形式类小亮点（仍不如规则 1.0） |
| 延迟 | **p50 226ms / p95 621ms** | 远超官方 ~33ms（GPU/短输入口径）；CPU 上比 embedding 15ms 慢 15 倍 |
| 概率接口 | **完整**：5 标签 probabilities + confidence + answer_confidence | **接口形态正是路由所需**（用户"返回置信度"诉求的正确实现） |
| 校准 | **警告："temperatures invalid…treat confidence as uncalibrated"** | conf≥0.5 准确率反降到 0.500；存在 conf 0.775 高置信错误（中文句→translate）——阈值分层不可靠 |
| conf 分层 | match 0.558 vs miss 0.378 | 有区分迹象但校准前不可用；错误方向上低置信正确（英文长句→none 时 conf 0.04~0.10，"知道自己在猜"） |

定位：**laya base 零样本不能直接用**；它的价值是「微调后语义层」的最佳载体候选——
概率接口 + 中文 + 322M + Apache 2.0 + 官方微调工具链（RLCD 校准），与 Needle 微调
同受语料前置约束。**零样本场景 embedding 路线仍居首**（natural_zh 0.83、15ms）。
LM Studio 侧 GGUF 仅包 backbone，decision head 为自定义结构，非 llama.cpp 可跑形态。

**LLM logprobs 补测**：LM Studio `logprobs/top_logprobs` 实现不完整（top_logprobs
的 probability 恒 0），无法做标签分布排序——「让 LLM 返回置信度排序」在当前
runtime 不可行，判别式模型才是概率的正确来源。


**few-shot 变体系统测试（`phase6c_fewshot.py`，qwen3.5-0.8b，示例均为 dataset 外新样本）**：
修正 §六"few-shot 无效"的结论——英文 3 示例（Q2/F3）确实有害，但中文为主 5 示例
（F1）是生成式路线全场最佳：

| 姿势 | overall | natural_zh | mixed | unparsed | 延迟 p50 |
|---|---|---|---|---|---|
| F1 中文为主 5 示例 | **0.646** | **0.33**（0→0.33 破冰） | 0.67 | 5/48 | 126ms |
| F2 每类 2 例（10 示例） | 0.542 | 0.5 | 0.67 | 7/48 | 131ms |
| F3 英文 3 示例（Q2 复跑） | 0.562 | 0.0 | 0.0 | 10/48 | 127ms |

- **中文示例确实能注入产品语境映射**（natural_zh 破冰、mixed 4/6、隐含翻译请求首次
  判对）——few-shot 是被 Q2 的坏姿势（英文示例）低估的路线。
- 但三个硬伤仍在：① **天花板 0.646** 仍远低于形式层规则；② **示例同时是"演示"与
  "干扰内容"**——unparsed 明细显示模型对「帮我总结一下这段话」输出了对 few-shot
  示例本身的总结（"用户分享了一句关于巧克力盒的名言…"），0.8B 无法维持任务边界，
  5/48 直接脱离标签输出；③ **剂量敏感且不可复现**——5 例甜点、10 例 url 类崩到
  0.33；F3 与 Q2 同配置复跑类别分布大幅漂移（english_word 0.67→1.0、mixed 0→0.67），
  temperature=0 下输出仍不稳定。
- 定位：不改变三层混合结论；但 0.25 语义层候选清单更新为——embedding（natural_zh
  0.83、15ms、稳定）居首，**qwen few-shot（0.646）成为"零新增模型资产"的生成式
  备选**（前提是接受 126ms 异步 lane 与输出稳定性风险）；few-shot 0.646 同时是
  微调预期的参照点——0.8B 容量足以容纳映射，微调大概率能推更高，语料前置不变。

**结构化输出 + JSON few-shot 组合（`phase6d_structured.py`，20260928 追加）**：
对策 F1 的 unparsed 边界破坏。实测 **LM Studio 支持 `response_format` json_schema**
（constrained decoding，输出被 grammar 硬约束为合法 enum；json_object 模式该端点
不支持）：

| 组 | schema | JSON few-shot | overall | unparsed | natural_zh | mixed | long_sentence_en |
|---|---|---|---|---|---|---|---|
| G1 | ✅ | 无 | 0.604 | **0** | 0.33 | 0.67 | 0.5 |
| G2 | ✅ | 中文 5 例 | 0.604 | **0** | **0.83** | **0.83** | **0.0** |
| G3 | ❌ | 中文 5 例 | 0.604 | **0** | 0.83 | 0.83 | 0.0 |

- **格式问题彻底解决**：unparsed 三组全 0。且 G2=G3 完全一致——schema 只保证格式，
  **JSON 示例本身就足以教格式**（schema 是产品级双保险而非准确率来源）。
- **G2 是生成式最强产品形态**：natural_zh 5/6、mixed 5/6（均创全场新高，追平/超过
  embedding 路线），输出恒可解析。但 **long_sentence_en 0/6 全崩向 ask_ai**——中文
  示例教会模型"自然语言句子→ask_ai"的**过宽规则**，英文长句也是自然句故全军覆没：
  逐类撬动此消彼长的模式在结构化输出下依旧。
- 定论：0.8B 在**格式维度绰绰有余**（用户判断成立），在**语义先验维度仍不足**
  （"英文自然句=该问助手"的内容先验只有微调或规则 CJK 短路能翻转）。若 0.25 走
  生成式语义层，产品组合 = json_schema + 中文 JSON few-shot + 规则先短路
  long_sentence_en/natural_zh（CJK 检测），模型只裁 mixed 歧义——此时 G2 的
  mixed 5/6 是该领地全场最佳。

**Needle 同法补测（`phase5b-needle-fewshot.json`，20260928）**：对 Needle 施加同款
组合（v2 语义 + forced + system 塞中文 6 示例）——overall **0.312**，比无 few-shot 的
E4（0.396）**更差**，natural_zh 仍 0.0，english_word/structured/path 全崩，延迟升至
474ms（system 变长 prefill 加重）。**Needle 用结构化+few-shot 方式提升不了**：① 它的
tool-call 协议自带 grammar 约束，`--forced` 已近似"强制分类"（E4 封顶 0.396）；
② **few-shot 对分布外语言无效**——0.8B 能被中文示例教会的前提是模型本身认识中文
（tokenizer/语料打底），Needle 对中文是分布外输入，示例演示无从泛化。

**中文超小模型生态位调研（20260928）**：**不存在"<500M 且原生 tool-calling 的中文
开源模型"**——Qwen2/2.5-0.5B 中文原生但 tool calling 弱/无原生支持，MiniCPM 最小 2B
（tool calling 可靠的实用下限），qwen3.5-0.8B 是国产最接近者。**但该生态位空缺不是
问题**：三层架构下"内置小引擎"的正确形态不是 tool-call LLM 而是**中文判别式小模型**——
bge-small-zh-v1.5（智源 24M 中文 embedding）、中文 BERT-mini/TinyBERT（几~30MB）、
mDEBERTa-v3 多语言 NLI（278M 零样本分类）、laya-multilingual（322M 决策模型）——
经 ONNX + OnnxRuntime InProcess 内置（**Blink PP-OCR 先例：零子进程、零 HTTP、
资产可嵌 .exe**），10~30MB、CPU 5~20ms，才是"小巧引擎内置"的可落地路径。

## 十、不建议现在做的事

- 不要把 needle.exe 纳入 EngineManager/引擎页（方案 A）——数据面 no-go 前提下增量面不成比例。
- 不要用 system prompt 继续调中文语境——已实测反效果（0.375→0.312）。
- 不要为 needle 开放引擎服务端口或直连 capability——无鉴权，违反信任边界。
- 不要据本 spike 手工 corpus 任何数字做 Adopt 决策——48 样本是我们自己出的题，正式
  评估仍须 0.24.3 线上观测分布（循环论证风险原文有效）。
