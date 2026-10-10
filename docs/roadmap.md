# Blink Roadmap

> **状态**：🔮 未完成方向池，不是版本承诺，也不预占版本号。
>
> **维护规则**：只保留尚未完成的方向；已立项内容迁入 `phases/`，已完成内容直接移出本文件。
>
> **当前判断**：短期优先保护主链路、补齐现有 Agent 交互与 Provider 适配；不立项 RAG、向量数据库或记忆向量化。

---

## 一、优先级与进入 Phase 的条件

候选方向依次判断：

1. 是否保护 `Alt+Space → Focus → 输入 → 首个结果` 主链路；
2. 是否让现有能力形成闭环、减少搬运和心智切换；
3. 是否复用统一能力底座，而非新增平行系统；
4. 是否默认本地、按需启用，并守住常驻内存与安装体积；
5. 是否有真实场景和可验证收益。

只有用户价值、最小操作路径、技术边界和验收范围均明确后，候选才迁入 `phases/{version}-*.md`。

---

## 二、未完成方向总览

| 方向 | 状态 | 当前结论 | 进入 Phase 的条件 |
|---|---|---|---|
| 主链路可靠性与性能 | **持续优先** | 始终高于新功能 | 出现焦点、唤起、输入或首结果回退时立即处理 |
| 跨 Provider 深度思考模式适配 | **近期优先** | 当前按钮默认开启，实时 reasoning 展示已接通；请求侧尚缺统一、诚实的 Provider/模型能力映射 | 明确 OpenAI-compatible/vLLM/DeepSeek、Anthropic、Gemini、Ollama 的开启/关闭协议、能力声明、custom params 覆盖和历史元数据语义 |
| Agent 能力增强 | **条件候选** | 集中评估路径级授权、`write_text_file`、受控 `read_url` 与长任务边界 | 出现稳定的“读取→修改→保存”或“URL→正文→处理”高频场景 |
| 多类型 API 扩展（生图/TTS 等） | **条件候选** | 语言大模型 API 为主；生图端点覆盖差（DeepSeek/Anthropic 无），TTS 未立项 | 支持配置独立类型的 API 端点（生图/STT/TTS）后，再评估 rig image_generation/audio_generation 接入收益 |
| rig 原生国产 provider 方言适配 | **条件候选** | factory 内按供应商选 dialect（DeepSeek tool_choice 抑制、Moonshot required 降级等）；zhipu/doubao/siliconflow 无方言仍走通用层 | 出现厂商兼容性实际问题；0.26.1 的 /responses 路由核验已先行 |
| 剪贴板历史音频类型 | **条件候选** | 监听器仅采集 CF_UNICODETEXT/CF_DIB/CF_HDROP | 音频格式（CF_WAVE 等）的采集、存储与预览成本评估清楚 |
| 极轻量文件快速预览 | **条件候选** | 收敛为文本/图片两类的轻量预览；复用 pin 窗口体系（blink-pin:// 协议）与 sticky 轻窗形态 | 文件选中感知（0.26.3）落地后出现稳定预览诉求 |
| 结构化输出抽取（rig extractor/prompt_typed） | **条件候选** | 标题生成现状鲁棒、无必需场景；依赖 `rig_core::schemars` re-export，落点为 domain::ai 新模块（受 rig 隔离墙约束，不能进 AIProvider trait） | 出现首个真结构化输出需求（意图兜底、批量操作确认等） |
| per-conversation MCP 工具限定 | **条件候选** | 工具池现为全局 disabled_tools 过滤 + epoch 重建 | /mcp scoping 涉及 agent 组装链路与 epoch 语义，需单独设计 |
| 外部 agent 作为 subagent | **条件候选** | 可借力长任务生态，但不能把 Blink 变成 agent 壳 | 文件长任务成为高频核心场景，且权限范围可控 |
| 主动建议深化 | **条件候选** | 坚持弱信号 pull、不 push | 有低误报、可关闭、能稳定减少至少一步操作的场景 |
| 配置同步（WebDAV） | **条件候选** | 多设备分发阶段再评估 | 多设备需求成立，并先确定加密与冲突策略 |
| AI 生成 Skill | **条件候选** | 现有纯文本模板可用，不急于引入 LLM 生成链 | 模板准确率成为真实瓶颈，且生成结果有审核闭环 |
| 事实记忆（tool-based） | **远期观察** | 与向量检索解耦，先证明显式长期记忆的用户价值 | 写入、查看、纠正、删除的产品模型明确 |
| 记忆向量召回 | **远期观察** | FTS5 + token-aware 窗口继续作为正式方案 | 有数据证明关键词召回无法满足真实对话 |
| RAG 本地知识库 | **远期观察** | 短期不做摄取、分块、embedding 与索引生命周期 | 本地知识问答成为高频场景且轻量方案不足 |
| 完整多模态模型投影 | **远期观察** | 图片输入（vision）已立项 0.26.10；输出侧与 document/audio 内容类型投影仍远期 | 模型输出侧多模态相对结构化结果引用有显著收益 |
| 完整 QuickLook 式预览 | **不建议** | 解码器、体积和维护成本偏离核心定位；极轻量文本/图片预览已单列为条件候选 | — |

---

## 三、近期优先

### 3.1 主链路可靠性

任何候选工作都不能牺牲：唤起 `< 50ms`、首个结果 `< 20ms`、焦点成功率 `> 99.9%`、常驻内存 `< 300MB`。出现回退时，新功能让位于主链路修复。

### 3.2 跨 Provider 深度思考模式适配

目标是让“深度思考”成为真实的请求语义，而不只是 reasoning 流的显示开关。正式立项前需解决：

- 建立模型级 `Supported / AlwaysOn / Unsupported` 能力表达，不能用空 JSON 混淆“不需要参数”和“不支持”；
- 按 Provider/模型映射开启与关闭参数，不向所有 OpenAI-compatible 服务盲发同一字段；
- 覆盖 vLLM、DeepSeek、Anthropic、Gemini、Ollama，并以实际 HTTP wire 验证；
- 本次请求的按钮状态优先于模型静态 custom params，且切换不触发 Agent 重建；
- Provider 无法关闭推理时如实呈现，不宣称关闭成功；
- 消息级保存发送时的思考状态，历史展示与导出不读取当前按钮状态；
- 即使 Provider 忽略关闭参数，前端仍防御性隐藏 reasoning。

当前过渡态：对话窗口按钮默认开启；rig 0.42 已能统一解析 reasoning/reasoning delta，但请求控制尚未完成上述跨 Provider 适配。

---

## 四、条件候选

### 4.1 Agent 能力增强

路径级授权、`write_text_file`、受控 `read_url` 与长任务能力应集中设计，不能零散扩大权限。

- 文件能力需定义规范化路径、symlink/reparse point、目录递归、授权失效和“用户指定/模型发现”边界；
- 写文件需明确原子写入、默认不覆盖、覆盖确认、编码、大小限制和失败恢复；
- URL 读取需明确响应上限、超时、重定向、正文抽取、内容类型及 SSRF 边界；
- Capability 保留 canonical data，AI 出口自行适配上下文，不在通用结果层做隐式全局截断。

### 4.2 外部 agent 作为 subagent

只将外部 agent 包装成受控粗粒度 tool，不把其作为 Blink 执行后端。立项前需明确工作目录和可写范围、失败降级、分钟级任务的进度/中止、授权粒度及非编码高频场景。

### 4.3 主动建议深化

弱意图信号只产 Ghost/Suggestion，由用户采纳；不抢首屏、不直接执行副作用、可一键关闭。只有建议能稳定减少至少一步操作且误报足够低时才立项。

### 4.4 配置同步

多设备需求成立后再评估 WebDAV。首期仅考虑低风险配置；历史、剪贴板图片、便签等用户内容必须另行设计加密、冲突解决与删除传播。

### 4.5 AI 生成 Skill

只有真实工具证明纯文本解析准确率不足时，才评估让 LLM 补全语义；生成物必须可预览、编辑和确认，不能静默成为执行指导。

### 4.6 多类型 API 扩展（生图/TTS 等）

生图与 TTS 不随语言大模型 provider 混做：rig image_generation 仅覆盖 OpenAI 系/Gemini/Venice（硅基流动需 additional_params 方言），DeepSeek/Anthropic 无生图端点，收益配不上集成面（0.26 规划时移出）。立项条件是先支持"按类型独立配置 API 端点"（生图/STT/TTS 各自的 provider/base_url/key 配置面），再评估接入；届时生成结果走 ResourceStore image_ref → pin_image/写剪贴板消费链路。

### 4.7 rig 原生国产 provider 方言适配

rig 0.43 国产"原生 provider"实为 openai 方言机制（DEEPSEEK/MOONSHOT/MINIMAX/XIAOMIMIMO 常量 + quirks），不新增 ProviderKind 也能在 factory 内按供应商选 dialect（`OpenAIConfig::with_key(dialect, key)`，with_base_url 正交保留）。实际增量收益：DeepSeek 的 tool_choice 抑制与消息拍平、Moonshot required 降级、DeepSeek 缓存命中 token 统计。zhipu/doubao/siliconflow/aliyun 在 rig 无方言，仍走通用层——双轨心智是主要成本。前置：0.26.1 的 /responses 路由核验完成。

### 4.8 剪贴板历史音频类型

剪贴板监听当前只采集文本/图片/HDROP 首文件名三种格式，音频（CF_WAVE/CF_RIFF）完全无处理。补齐需评估：格式枚举与转码（WAV/MP3）、blob 存储体积（对照 clipboard_images 200 条上限策略）、预览与播放 UI、以及"发送到聊天窗口"作为音频附件的联动。

### 4.9 极轻量文件快速预览

不做完整 QuickLook（解码器、体积和维护成本偏离核心定位）。收敛方向：仅文本与图片两类的极轻量预览——图片复用 pin 窗口体系（blink-pin:// 自定义协议 + spare 预热），文本参考 sticky/voice-overlay 的轻量无框窗形态。前置：文件选中感知（0.26.3）落地后验证真实预览诉求。

### 4.10 结构化输出抽取（rig extractor / prompt_typed）

rig-agent ExtractorBuilder 提供 schema 约束输出 + 可配置重试（含"模型拒调 submit 工具时宽容解析最终文本"的 native 模式 prompt_typed）。受 rig 隔离墙约束只能落在 domain::ai 新模块（泛型方法不能进 object-safe 的 AIProvider trait），依赖 `rig_core::schemars` re-export 零新增。当前无必需场景（标题生成现状鲁棒、编辑器整理是自由文本）；首个真结构化输出需求（意图兜底、批量操作确认）出现时立项。

### 4.11 per-conversation MCP 工具限定

`/mcp` 查看已在 0.26.7 落地；"限定当前对话只用某个 MCP 来源"是另一件事：工具池现为全局 disabled_tools 过滤 + epoch 触发 Agent 重建（chat_service 组装时 collect_tools 无 conversation 维度参数）。对话级白名单需要 InvokeContext/agent 组装链路带 conversation 级 tool 过滤并重新定义缓存键语义，单独设计后再立项。

---

## 五、远期观察：向量、RAG 与长期记忆

现有 token-aware 窗口 + FTS5 记忆召回是正式方案。当前没有数据证明语义召回是主要瓶颈；向量化会引入模型、索引、重建、后台任务、失败恢复、隐私告知和设置 UI，不作为局部增强提前建设。

未来重评必须同时满足：

1. 真实高频用例证明 FTS5 或普通文件搜索明显不足；
2. 有可量化的离线评测集与收益门槛；
3. 实现满足 Rust、Windows、安装体积和常驻内存约束；
4. 来源、更新、删除、引用和隐私告知生命周期完整；
5. 后台索引不影响主链路，关闭后可彻底删除数据。

若未来实现，必须保持无 embedding 时完整可用、后台旁路化、模型身份锁定、本地优先、可降级/可删除，并通过 Capability 进入既有 tool loop。

事实记忆应先验证显式写入、读取、纠正和删除的产品模型，不因“想做记忆”顺带引入向量基础设施。
