# production-design / 产品设计文档

> Blink 所有产品设计决策与架构留档。**改核心前先读本文件，再按需深入。**
>
> 📖 **产品总纲入口**：请先阅读 [00-overview.md](./00-overview.md) 了解产品定位、里程碑与完整文档体系。

---

## 这是什么

不是代码，是**"为什么这样设计"的留档**——产品方向、交互取舍、架构决策、各版本演进。
代码里的设计意图散落各处易遗失；此目录是 single source of truth，决策争议回溯至此。

---

## 目录结构

```
production-design/
├── README.md                      ← 本文件:目录导航 + 运作规则
├── 00-overview.md                 ← ⭐️ **入口必读**:产品愿景 + 技术架构 + 里程碑总览
│
├── product-interaction.md         ← 产品卷①:定位/唤起/热键/搜索/右键菜单/Chord模式/i18n
├── product-platform.md            ← 产品卷②:插件surface模型/意图路由/AI能力
├── product-context-future.md      ← 产品卷③:Context环境感知/主动建议/隐私安全
├── product-principles.md          ← 产品卷④:已知取舍/日志规范/演进时间线
│
└── phases/                        ← 版本档案:每个版本的架构设计 + 实现总结
    ├── 0.1-base.md                     ← MVP基础交互
    ├── 0.2-core-plugin-design.md       ← ⚠️ 改核心前必读(Service/SearchEngine/Plugin/Intent)
    ├── 0.3-plugin-skeleton.md          ← ⚠️ 改核心前读(插件骨架 + 热键物理态重构)
    ├── 0.4-intent-router.md            ← 意图路由层
    ├── 0.5-config-search-extension.md  ← 配置架构 + 文件搜索 + 扩展系统
    ├── 0.6-plugin-packaging-scripting.md ← 插件打包 + Python/Node.js脚本支持
    ├── 0.7-plugin-ecosystem-local-search.md ← 插件生态 + 本地搜索Fallback + 性能统计
    └── 0.8-ai-intent-router.md         ← 🚧 进行中:UIA划词 + Chord模式 + AI能力基础
```

---

## 三层文档，各司其职

| 层 | 文件 | 回答 | 性质 |
|---|---|---|---|
| **总纲** | `00-overview.md` | 产品是什么、整体架构、路线图 | 入口必读 |
| **产品决策** | `product-*.md` 四卷 | **为什么**这样设计(交互/扩展/感知/原则) | 留档，争议回溯处 |
| **技术实现** | `phases/*.md` | **怎么做**(各版本架构设计 + 实现总结 + 已知问题) | 改核心前必读 |

---

## 运作规则

### 新增一个产品决策时
1. 判断属于哪个域 → 写进对应 `product-*.md`
   - 交互/热键/搜索体验 → `product-interaction.md`
   - 插件/意图/AI → `product-platform.md`
   - Context/隐私/未来演进 → `product-context-future.md`
   - 横切取舍/工程规范/时间线 → `product-principles.md`
2. 标注**来源**(哪个 phase 提出)
3. 若该决策当期落地，同步进 `phases/{version}-*.md` 的里程碑

### 改核心前读什么
1. 本 README（知道有什么、在哪）
2. `00-overview.md`（全局架构与既定方案）
3. 相关 `product-*.md`（为什么）
4. 相关 `phases/*.md`（实现细节与已知问题，尤其 0.2/0.3 标"改核心前必读"）

### 版本档案命名
`phases/{version}-{topic}.md`
- 已完成版本 = "实现总结 + 已知问题 + 后续方向"
- 进行中版本 = "设计 + 工作项 + 验收标准"

### 文档间引用约定
- **纯文件名 + §号**(如 `0.2-core-plugin-design.md §3`、`product-platform.md §4.3`)：同目录或语义引用，可读性优先
- **拆分历史**：四卷 `product-*.md` 由原 `product-design.md` 拆出，**保留原 § 节号**(如 platform 卷从 §4 起)，保证交叉引用稳定
- **改文件名/移动时**，全局搜旧引用并同步更新(CLAUDE.md、src 代码注释、文档间都会引用)
- `00-overview.md` 即原 `MVP.md`，文中"MVP §X"均指此文件

### 与代码的关系
- `CLAUDE.md` 是**给 Claude Code 的工作指引**(怎么改代码)；本目录是**产品设计留档**(为什么这么设计)。两者互补，CLAUDE.md 已链入本目录。
- 代码注释里"见 product-platform.md §4.3 / phases/0.2-... §3"即指回此目录。

---

## 演进约定

每个大版本完成后：
1. 在 `phases/` 新增 `{version}-{topic}.md` 做实现总结
2. 产品决策沉淀进对应 `product-*.md`
3. 总纲级变化更新 `00-overview.md`
