# Needle Router Spike

0.24.6 的去风险可行性实验（phase 文档 §5.8），只回答四个问题，不做生产接入：

1. **运行形态**：Needle 3 官方 Windows runner（`needle.exe`）能否常驻（`--serve` HTTP）？
   与单发（`--prompt` 每请求冷启）相比延迟/资源差多少？
2. **数据集适用性**：Blink 主窗口典型输入（URL / 本地路径 / 英文单词（应用名）/
   英文长句 / 中文自然语言 / 明确翻译 keyword / 结构化文本 / 混合语言）上，
   Needle 的 5 类路由（translate/ask_ai/open_url/open_path/none）是否可用？
3. **协议合同**：`--serve` 的端点行为与 Blink `EngineManager` 健康合同差多少？
4. **接入路径**：进 EngineManager（引擎化）还是 `ModelIntentProducer` + infra 原语复用？

结论见 `decision.md`。

## 运行

```bash
cd xtask/spikes/needle-router
python run_spike.py            # 主 spike（phase 1~4：形态/数据集/协议），约 3 分钟
python phase5_posture.py       # 姿势归因实验 E1~E6（归因见 decision.md §七）
python phase6a_qwen_local.py   # 跨模型对照：LM Studio qwen3.5-0.8b（§八；需 LM Studio
                               #   跑于 127.0.0.1:1234 且加载 qwen3.5-0.8b）
python phase6c_fewshot.py      # few-shot 变体系统测试（§九；同上依赖）
python phase6d_structured.py   # 结构化输出组合测试（§九；LM Studio json_schema 支持）
python phase6b_embedding.py    # embedding 标签相似度（§九；需 LM Studio 加载
                               #   text-embedding-nomic-embed-text-v1.5）
python phase7_laya.py          # laya 决策模型（§九；需 `python -m venv .venv &&
                               #   .venv/Scripts/pip install laya`，首次下载 647MB——
                               #   HF 直连/镜像均被网络阻断时不可跑）
python run_spike.py --skip-oneshot   # 跳过阶段 1（单发基准）
```

- 首次运行自动从 HuggingFace `Cactus-Compute/needle3` 下载 runner/权重/tokenizer 到
  `.assets/`（约 37MB，带大小校验；该目录被 gitignore，制品不入库）。
- 结果写 `results/spike-results.json`；控制台打印摘要。
- 需要 Python 3.8+（仅标准库）与 Windows x64。

## 范围边界

- 期望标签以 **0.24 规则版语义**为准（如英文自然长句→translate 建议面），
  不代表 Needle 官方语义；两组 system prompt（minimal/enriched）对照，
  回答"中文语境误判能否靠 prompt 解决"。
- 不评估 macro-F1（正式 Adopt 的验收仍按 0.24 §5.7 走线上观测基线）。
- 不修改 `src/`、`frontend/` 任何产品代码。
