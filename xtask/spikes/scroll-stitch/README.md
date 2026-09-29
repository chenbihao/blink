# scroll-stitch spike：长截图拼接算法对照实验

> 目的：验证调研提出的三个改进方向（鲁棒 SAD / 相位相关 / 混合）能否解决
> 现有 SAD 匹配在悬浮元素、周期内容、小重叠下的"断崖式失败"。
> 结论见 [decision.md](./decision.md)。

## 跑法

```bash
cd xtask/spikes/scroll-stitch
cargo run --release
```

零外部依赖、独立于主 workspace。合成场景确定性可复现（固定种子）。

## 被测算法

| 算法 | 说明 |
|---|---|
| `baseline` | 生产 `stitch.js estimateVerticalShift` 的忠实移植（含其精搜 skip 行为） |
| `baseline-fix` | 同上，但修正精搜 skip 规则（量化生产 bug 的影响） |
| `robust` | 双侧行变化掩码 + 截尾 SAD 选候选，全量（48 行）SAD 做接受判定与多义守门 |
| `phase` | 1D 行投影相位相关（手写 FFT），峰比多义检测 + 抛物线亚像素 |
| `phase-mask` | 同上，投影前用行变化掩码剔除固定行 |
| `hybrid` | 掩码 PC 候选 ∪ 鲁棒全搜最优 → ±4px 鲁棒精修 → 全量验证 |

## 场景

合成文档 1200×(7000~15000)，视口 1200×800，帧间真值位移已知，采集噪声 ±2/通道。

| 场景 | 内容 | 位移(重叠) | 考察点 |
|---|---|---|---|
| plain-70 | 密集文本 | 240 (70%) | 基线 sanity |
| sticky-70 | +吸顶栏+置底栏 | 240 (70%) | 固定层 |
| plain-31 | 密集文本 | 550 (31%) | 小重叠 |
| sticky-31 | +吸顶+置底 | 550 (31%) | 固定层+小重叠 |
| float-31 | +悬浮按钮+光标闪烁 | 550 (31%) | 悬浮/动态元素 |
| all-31 | 全部固定层 | 550 (31%) | 断崖现场 |
| all-20 | 全部固定层 | 640 (20%) | 超出 0.78H 搜索上限 |
| lowdetail-31 | 大片空白稀疏内容 | 550 (31%) | 信息不足 |
| repeated-31 | 完全周期列表(96px) | 550 (31%) | 多义 |
| all-80 | 全部固定层 | 80 (90%) | 小步进 sanity |

判定口径：`exact`（整数精确）/ `near`（±1）/ `wrong`（接受但错误 = 生产中
静默损坏）/ `rej`（安全拒绝，amb 标记因多义拒绝）。

## 文件

```
src/frame.rs          合成文档/固定层/噪声/帧序列生成
src/algo/baseline.rs  生产算法移植 + skip 修正变体
src/algo/robust.rs    鲁棒 SAD（掩码/截尾/全量验证/多义守门）
src/algo/phase.rs     FFT + 行投影相位相关
src/algo/hybrid.rs    混合管线
src/main.rs           场景编排 + 记分 + phase 符号自检
```
