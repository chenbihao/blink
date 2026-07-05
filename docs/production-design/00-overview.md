# Blink — 产品总览与文档导航

> **定位**: Universal Action Layer（统一操作层）—— 感知用户上下文、主动推荐动作,让任何操作都比原来的路径更快。
>
> **状态**: 0.7 已完成,0.8 进行中（0.8.0 ~ 0.8.5 完成；0.8.6 架构固化规划中；0.8.7 Alt+A 截图规划中），0.9 AI 层规划中
> **更新时间**: 2026-07-05

---

## 一、产品核心理念

### 1.1 不是启动器,是「统一操作层」

搜索只是入口之一,**动作执行才是终点**。

| 维度 | 说明 |
|---|---|
| **唤起方式** | 右 Alt 单击(tap)唤起,按住不松开进入 Chord 模式直接触发动作 |
| **核心体验** | 唤起→输入→执行,全程&lt;1秒,比鼠标/菜单更快 |
| **P0 至上** | 如果用户按快捷键后不能立即输入,其他所有功能都没有意义 |
| **演进方向** | 被动搜索 → Context 感知主动推荐 → AI 辅助操作 |

### 1.2 关键性能指标

| 指标 | 目标 |
|---|---|
| 快捷键唤起延迟 | &lt; 50 ms |
| 输入首个结果延迟 | &lt; 20 ms |
| 常驻内存 | &lt; 300 MB |
| 输入焦点成功率 | &gt; 99.9% |

---

## 二、文档导航

### 2.1 产品设计四卷

> 跨版本通用的**产品决策留档**（为什么这样设计）。每卷聚焦一个层面，可独立阅读。

| 文档 | 内容 |
|---|---|
| **[product-interaction.md](./product-interaction.md)** | ✅ **交互体验层** —— 产品定位、唤起/焦点/IME、搜索双 lane、右键菜单、Chord 模式、i18n |
| **[product-platform.md](./product-platform.md)** | 🔧 **平台扩展层** —— 插件系统 + 呈现权 surface 模型、**四域架构 + ExecArg 类型墙**、意图路由、**0.8.6 三个统一入口 trait**、AI 能力方向 |
| **[product-context-future.md](./product-context-future.md)** | 🔮 **感知与未来** —— Awareness 环境感知（UIA 划词/剪贴板/前台）、主动建议、隐私安全 |
| **[product-principles.md](./product-principles.md)** | 📋 **横切原则** —— 已知取舍、日志规范、演进时间线、**最小操作路径准则、可辨识度与视觉一致性** |

### 2.2 各阶段技术实现文档

> `phases/` 目录按版本沉淀**技术实现决策**——为什么这样做、踩过的坑、验收标准。

| 阶段 | 文档 | 核心内容 | 状态 |
|---|---|---|---|
| **0.1** | [phases/0.1-base.md](./phases/0.1-base.md) | P0 基础交互 + 热键/窗口/焦点/IME + 搜索 + 配置系统 | ✅ 完成 |
| **0.2** | [phases/0.2-core-plugin-design.md](./phases/0.2-core-plugin-design.md) | Service 骨架 + SearchEngine trait + 双 lane 搜索 + 图标懒加载 | ✅ 完成 |
| **0.3** | [phases/0.3-plugin-skeleton.md](./phases/0.3-plugin-skeleton.md) | 插件系统骨架(独立进程+stdio JSON)+ 热键物理态重构 | ✅ 完成 |
| **0.4** | [phases/0.4-intent-router.md](./phases/0.4-intent-router.md) | 意图引擎 RuleRouter + Context 层 + ip 插件 | ✅ 完成 |
| **0.5** | [phases/0.5-config-search-extension.md](./phases/0.5-config-search-extension.md) | 配置架构统一 KV + Everything 文件搜索 + 右键菜单 + 主题系统 | ✅ 完成 |
| **0.6** | [phases/0.6-plugin-packaging-scripting.md](./phases/0.6-plugin-packaging-scripting.md) | 插件打包路径 + Python/Node.js 脚本支持 + 统一错误处理 | ✅ 完成 |
| **0.7** | [phases/0.7-plugin-ecosystem-local-search.md](./phases/0.7-plugin-ecosystem-local-search.md) | 插件生态(翻译/剪贴板历史)+ 本地搜索 Fallback + 性能统计 | ✅ 完成 |
| **0.8** | [phases/0.8-context-interaction.md](./phases/0.8-context-interaction.md) | 感知与操作层 —— UIA 划词 + 内置动作抽象 + Autosuggestion + 翻译 Context 路由 + 四域架构 + Chord 交互（0.8.6 架构固化、0.8.7 补 Alt+A 截图） | 🚧 进行中 |
| **0.9** | [phases/0.9-ai-layer.md](./phases/0.9-ai-layer.md) | 智能层 —— AI Provider Trait + 云端插件 + Chat View + VectorRouter | 📋 规划中 |

### 2.3 其他参考

| 文档 | 说明 |
|---|---|
| **[CLAUDE.md](../../CLAUDE.md)** | 开发规范 + 模块拆分 + Tauri Commands 清单 + 常见问题 |
| **README.md** | 项目介绍、构建运行指南(项目根目录) |

---

## 三、版本里程碑速览

| 版本 | 核心特性 | 状态 |
|---|---|---|
| 0.1 ~ 0.7 | 基础交互 → 插件生态 → 剪贴板历史 → 性能统计 | ✅ 全部完成 |
| **0.8.0** | UIA 划词文本感知 + Context 感知路由（内置动作侧）+ 内置动作抽象升级 | ✅ 完成 |
| **0.8.1** | Autosuggestion / Ghost Text 输入补全（首拼降级） | ✅ 完成 |
| **0.8.2** | 翻译插件 Context 感知路由 + `needs_translation` + `PluginSettingResolver` trait | ✅ 完成 |
| **0.8.3** | 感知交互统一：`Suggestion` 抽象 + Context 转 Ghost 采纳 + 智能感知面板 + Ghost 本地化 + origin 提示 + **awareness 域重构** | ✅ 完成 |
| **0.8.4** | **四域架构重构**：route 断 Awareness + ExecArg 类型墙 + RankingHint Surface Booster + Suggestion 覆盖非空 query | ✅ 完成 |
| **0.8.5** | **Chord 模式底层能力**：注册机制 + 独立悬浮窗 + Alt+Q 划词 + Alt+C 剪贴板（EngineTakeover）+ 配置面板 | ✅ 完成 |
| **0.8.6** | **架构固化**：Action trait / SuggestionProducer + Arbiter / ConfigStore / SearchService 拆分 / AppContext 真依赖容器 / 内置动作 i18n（为 0.9 铺物理骨架，纯横向重构） | 📋 规划中 |
| **0.8.7** | Alt+A 区域截图 | 📋 规划中 |
| 0.9.0 | 最小 AI Provider Trait + 注册机制 | 后置 |
| 0.9.1 | 云端 AI 插件（OpenAI 兼容）+ 密钥安全存储 | 后置 |
| 0.9.2 | AI Chat View 对话界面 + `ai xxx` 触发 | 后置 |
| 0.9.3 | VectorRouter 语义匹配（embedding 可选云端/本地） | 后置 |

---

## 四、快速开始

**想了解某方面的设计?**

| 你想了解 | 看这里 |
|---|---|
| 热键为什么是右 Alt tap? Chord 模式怎么工作? | [product-interaction.md §2](./product-interaction.md) |
| 插件系统怎么设计的? 意图路由怎么实现? | [product-platform.md §4-5](./product-platform.md) |
| 四域架构（Awareness/Suggestion/Routing/Execution）铁则? | [product-platform.md §5.0](./product-platform.md) |
| 0.8.6 架构骨架（Action trait / SuggestionProducer / ConfigStore）? | [product-platform.md §7](./product-platform.md) + [phases/0.8 §八](./phases/0.8-context-interaction.md) |
| 选中文本/剪贴板感知怎么做?隐私如何保证? | [product-context-future.md](./product-context-future.md) |
| 最小操作路径准则 / 中文不斜体 / 键盘提示统一 | [product-principles.md §13-14](./product-principles.md) |
| 0.8 感知与操作层在做什么? | [phases/0.8-context-interaction.md](./phases/0.8-context-interaction.md) |
| 0.9 AI 层准备怎么落? | [phases/0.9-ai-layer.md](./phases/0.9-ai-layer.md) |
| 开发规范、模块拆分、Tauri Commands | [CLAUDE.md](../../CLAUDE.md) |
