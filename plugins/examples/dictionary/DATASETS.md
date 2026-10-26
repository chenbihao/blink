# Blink 词典数据集来源与许可（0.28.4）

数据集（词库）与插件包独立版本化、分开分发。本文件记录每个数据包的
数据来源、许可与构建剖面（phase 0.28 §五.4.1：来源可追溯、许可可核验、
构建可重复）。

## dictionary-basic（基础词库，必备）

| 项 | 值 |
|---|---|
| 数据来源 | [ECDICT](https://github.com/skywind3000/ECDICT) `ecdict.csv`（仓库内约 77 万词条） |
| 许可 | MIT（ECDICT 上游 LICENSE） |
| 格式版本 | 1 |
| 构建剖面 | `basic`（默认）/ `full` |
| 产物 | `dict.db`（entries/forms/zh_index 三表 SQLite） |

### basic 剖面裁剪规则（构建期完成，不是运行期过滤）

- **单词**（不含空白）：全保留——技术词（async/deprecated 等）不受语料
  频率限制；无译/无义项行剔除。
- **短语**：满足其一保留——
  - 频率信号：bnc / frq 任一 > 0，或有 tag（zk/gk/cet4…）/ collins 星 /
    oxford 3000；
  - 常见习语形态：原文全小写、≤4 个纯字母 token、总长 ≤24 字符。
  - 原文含大写字母的专名/标题短语（"A and B agglutinogens" 等）裁掉。
- **词形（forms）**：`exchange` 的 p/d/i/3/r/t/s 段登记为「本词的变形」；
  `0:`（lemma 原型）延迟回填为「变形 → 原型」（原型行可能字母序在前）。
- **中文反查（zh_index）**：只为带频率信号的条目建索引；键取译文前 3 行、
  每行 ≤2 个 1..=8 字纯 CJK 词节（去词性/领域前缀；"的"尾同时登记去"的"
  版本）。基础汉英覆盖常用概念，长尾词条的反查不在本剖面承诺内。

### full 剖面

全部有译/义项条目（含全部短语），无裁剪。

### 可重复构建与校验

- `dataset.json.content_sha256` = DB 行流（entries → forms → zh_index 全序）
  的 SHA-256——同源数据 + 同剖面重建必须得到相同摘要；
- `dataset.json.files[*].sha256` = 载荷文件 SHA-256（安装期逐文件核验）；
- 质量样本 `plugins/examples/dictionary/quality-samples.json`（固定公开样本：
  常用/多义/词形/短语/技术词/基础汉英），`--check-quality` 逐项校验，
  任一不达标构建即失败——上游数据漂移不允许静默降质。

## dictionary-etymology（词源增强包，可选）

| 项 | 值 |
|---|---|
| 数据来源 | [ECDICT](https://github.com/skywind3000/ECDICT) `wordroot.txt`（JSON：词根/词缀 → 释义/示例词） |
| 许可 | MIT（ECDICT 上游 LICENSE） |
| 格式版本 | 1 |
| 产物 | `ety.db`（word → roots[] 单表 SQLite） |

缺失不阻塞插件就绪（manifest 声明 `optional: true`）；词条详情在词源包
就绪时附加词根信息，缺失时如实留空——不以 AI 生成填充事实字段（phase
§3.4）。

## 构建命令

~~~bash
# 基础词库（默认 basic 剖面）
cargo xtask dictionary --build-basic --ecdict <ecdict.csv> --version 1.0.0 \
  --out target/dictionary
# 词源包
cargo xtask dictionary --build-ety --wordroot <wordroot.txt> --version 1.0.0 \
  --out target/dictionary
# 校验 + 质量样本
cargo xtask dictionary --verify target/dictionary/dictionary-basic/1.0.0
cargo xtask dictionary --check-quality target/dictionary/dictionary-basic/1.0.0
~~~

ECDICT 源文件从上游仓库获取（`ecdict.csv` / `wordroot.txt` 位于仓库根，
MIT 许可）；数据发布流水线（官方目录下载）在 0.28.5 接入，当前通道为
本地构建 + 设置页手动安装（.zip）。
