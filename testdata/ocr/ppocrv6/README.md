# Golden Corpus — PP-OCRv6 Spike

## 许可

所有 corpus 图片和标注文件使用 [CC0-1.0](https://creativecommons.org/publicdomain/zero/1.0/)（公共领域）许可。可自由再分发。

## 来源

所有图片由 `generate_corpus.py` 程序生成，不含私人截图或敏感内容。

## 覆盖范围

| 子集 | 语言 | 方向 | 说明 |
|---|---|---|---|
| `chinese` | zh | horizontal | 中文基本文本 |
| `english` | en | horizontal | 英文基本文本 |
| `japanese` | ja | horizontal | 日文基本文本 |
| `mixed` | zh+en | horizontal | 中英混排 |
| `vertical` | ja/zh | vertical | 竖排文本 |
| `small-font` | zh/en | horizontal | 小字号（12px） |
| `light-ui` | zh/en | horizontal | 浅色背景 + 深色文字 |
| `dark-ui` | zh/en | horizontal | 深色背景 + 浅色文字 |
| `medium` | mixed | horizontal | 1440p 截图替代（benchmark 用） |
| `dpi` | en | horizontal | 不同 DPI（100/150/200%） |

## 生成

```powershell
python .\xtask\spikes\ppocrv6\generate_corpus.py --output .\testdata\ocr\ppocrv6\
```

生成脚本使用 Pillow (PIL) 程序化生成图片，不依赖任何外部素材。

## 标注

期望文本存储在 `manifest.json` 的 `expected_text` 字段中。

几何判定规则（word rect 验证）：
- rect 非空：`w > 0` 且 `h > 0`
- rect 有限：所有字段 < 1e9
- rect 在图像范围内：`x >= 0 && y >= 0 && x + w <= image_width + 5 && y + h <= image_height + 5`（5px 容差）
- 有效 rect 比例 = 有效 rect 数 / 总 rect 数
