# Blink 词典数据集来源与许可（0.28.4；0.28.13 客户端自举）

数据集（词库）与插件包独立版本化。本文件记录每个数据包的数据来源、
许可、构建剖面与源锁定（phase 0.28 §五：来源可追溯、许可可核验、
构建可重复）。

**0.28.13 起，词库不再由我们构建托管**：发布 CI 不拉 ECDICT、不产数据包
zip/catalog 条目。词典插件安装后由客户端自举——宿主按插件包内
`bootstrap.json` 的源锁定声明下载源文件（哈希核验），插件进程在本机
构建（含质量样本门禁），经宿主挂载。安装来源记 `managed`
（插件版本 + 源修订 + 操作 id 溯源），不冒充官方预构建包。

## 源锁定声明（bootstrap.json）

| 资源 id | 文件 | 上游修订（固定 commit） | sha256 | bytes | 许可 |
|---|---|---|---|---|---|
| `ecdict-csv` | `ecdict.csv` | `8defb761f7c7ad1818ca94290a1844d7b33d6b23` | `d0ce61e560b50d9905d20de3173aa3ca80950ce235bedd21c53e025cf9f38cb0` | 65,936,699 | MIT |
| `wordroot-txt` | `wordroot.txt` | `8defb761f7c7ad1818ca94290a1844d7b33d6b23` | `4b400dc5980e4a362fa12c7a49b326bb621c4e6bebd5f37146fbdcbe9db6bb6e` | 370,702 | MIT |

锁定规则（phase 0.28 §五.0.28.13）：只允许固定 commit 的 raw HTTPS 地址
（禁 master/latest 可变引用）；镜像（jsDelivr raw 改写/GitHub 代理）必须
返回与锁定 sha256 相同的字节，校验失败不得进入构建；实际写入以声明
bytes 为硬上限（单文件 128 MiB / 总量 512 MiB 基线内）。

## dictionary-basic（基础词库，必备）

| 项 | 值 |
|---|---|
| 数据来源 | [ECDICT](https://github.com/skywind3000/ECDICT) `ecdict.csv`（约 77 万词条） |
| 许可 | MIT（ECDICT 上游 LICENSE） |
| 格式版本 | 1 |
| 自举构建版本 | 1.1.0（manifest `datasets.min_version` 同步） |
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
- `dataset.json.files[*].sha256` = 载荷文件 SHA-256（挂载期逐文件核验）；
- `dataset.json.source.revision` = 源锁定声明的上游 commit SHA（宿主把它
  记入 `InstallSource::Managed` 溯源）；
- 质量样本 `plugins/dictionary/quality-samples.json`（固定公开样本：常用/
  多义/词形/短语/技术词/基础汉英）在自举构建期**内嵌**进插件二进制并
  逐项校验，任一不达标自举即失败——上游数据漂移不允许静默降质。

## dictionary-etymology（词源增强包，可选）

| 项 | 值 |
|---|---|
| 数据来源 | [ECDICT](https://github.com/skywind3000/ECDICT) `wordroot.txt`（JSON：词根/词缀 → 释义/示例词） |
| 许可 | MIT（ECDICT 上游 LICENSE） |
| 格式版本 | 1 |
| 自举构建版本 | 1.1.0 |
| 产物 | `ety.db`（word → roots[] 单表 SQLite） |

缺失不阻塞插件就绪（manifest 声明 `optional: true`）；词条详情在词源包
就绪时附加词根信息，缺失时如实留空——不以 AI 生成填充事实字段（phase
§3.4）。

## 客户端自举流程（0.28.13）

1. 安装词典插件（插件中心 zip；manifest 校验 `bootstrap.json` 合法性）。
2. 启用状态下宿主检查数据集三态：`missing / corrupt / outdated` 任一成立
   即触发自举（兼容已挂载且 ≥ min_version 的保留库直接跳过构建）。
3. 宿主按源锁定下载源文件（镜像候选降级 + sha256/bytes 校验）到插件
   cache 下独立操作目录；失败保留分类错误（network_failed / timeout /
   hash_mismatch / over_limit），不提交半成品。
4. 插件进程在工作线程构建（basic 约 22s/102MB 磁盘，ety <1s），服务循环
   保持响应；宿主杀进程即有界终止，无孤儿子进程。
5. 构建产物经逐文件校验 + 质量样本后，插件请求宿主挂载（受限相对路径）；
   宿主复核插件身份/数据集声明/路径边界后走既有安装状态机提交
   （active.json 原子切换），来源记 `managed`。
6. 可选词源独立准备/重试；其失败不撤销已就绪的基础库。

## 本地构建命令（开发/发布准入验证）

~~~bash
cargo xtask dictionary --build-basic --ecdict <ecdict.csv> \
  --version 1.1.0 --source-revision 8defb761f7c7ad1818ca94290a1844d7b33d6b23 \
  --out target/dictionary
cargo xtask dictionary --build-ety --wordroot <wordroot.txt> --version 1.1.0 \
  --out target/dictionary
cargo xtask dictionary --verify target/dictionary/dictionary-basic/1.1.0
cargo xtask dictionary --check-quality target/dictionary/dictionary-basic/1.1.0
~~~

锁定源文件本地获取（与 bootstrap.json 声明逐字节一致）：

~~~bash
curl -L -o ecdict.csv https://raw.githubusercontent.com/skywind3000/ECDICT/8defb761f7c7ad1818ca94290a1844d7b33d6b23/ecdict.csv
curl -L -o wordroot.txt https://raw.githubusercontent.com/skywind3000/ECDICT/8defb761f7c7ad1818ca94290a1844d7b33d6b23/wordroot.txt
~~~
