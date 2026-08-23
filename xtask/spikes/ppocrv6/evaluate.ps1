<#
.SYNOPSIS
PP-OCRv6 spike 评价脚本（CER / word rect 有效性 / WinRT 对比）

.DESCRIPTION
使用 testdata/ocr/ppocrv6/ 中的 golden corpus，输出：
- 各子集 CER（中文/英文/日文/竖排/小字号/浅色深色/DPI）
- 相对 WinRT 的变化
- 有效 word rect 比例（native vs fallback 分开统计）
- 越界/空 rect/DPI 偏移

使用 PaddleOCR 3.7 API（predict()）。

.NOTES
WinRT baseline 缺失时本脚本必须失败，不能跳过后仍视为评价完成。
WinRT baseline 由 winrt_baseline.rs 生成到 results/winrt_baseline.json。
#>

param(
    [string]$VenvDir = "$PSScriptRoot\.venv",
    [string]$ModelCacheDir = "$PSScriptRoot\model-cache",
    [string]$ResultsDir = "$PSScriptRoot\results",
    [string]$CorpusDir = (Resolve-Path "$PSScriptRoot\..\..\..\testdata\ocr\ppocrv6").Path,
    [string[]]$Models = @("tiny", "small", "medium"),
    [string]$Topology = "thin"
)

$ErrorActionPreference = "Stop"
$venvPython = Join-Path $VenvDir "Scripts\python.exe"

if (-not (Test-Path $venvPython)) {
    throw "venv 不存在，请先运行 install.ps1"
}

New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

# ── 检查 WinRT baseline 是否存在 ──
$winrtFile = "$ResultsDir\winrt_baseline.json"
$winrtAvailable = Test-Path $winrtFile

if (-not $winrtAvailable) {
    Write-Host "[ERROR] WinRT baseline 不存在: $winrtFile" -ForegroundColor Red
    Write-Host "       请先运行 WinRT baseline 生成程序。" -ForegroundColor Red
    Write-Host "       WinRT baseline 缺失时不能视为评价完成。" -ForegroundColor Red
    throw "WinRT baseline 缺失"
}

Write-Host "=== PP-OCRv6 评价 ===" -ForegroundColor Cyan
Write-Host "WinRT baseline: $winrtFile (已就绪)"

$ModelListPython = ($Models | ForEach-Object { "'$_'" }) -join ', '

# ── Python 评价脚本 ──

$evalScript = @"
import base64, json, os, sys, time, hashlib

# UTF-8 安全
sys.stdin.reconfigure(encoding='utf-8', errors='replace')
sys.stdout.reconfigure(encoding='utf-8', errors='replace', line_buffering=True)
sys.stderr.reconfigure(encoding='utf-8', errors='replace')

CORPUS_DIR = r'$CorpusDir'
MODEL_CACHE = r'$ModelCacheDir'
RESULTS_DIR = r'$ResultsDir'
MODELS = [$ModelListPython]

# ── 模型映射 ──
MODEL_MAP = {
    'tiny': {'det': 'PP-OCRv6_tiny_det', 'rec': 'PP-OCRv6_tiny_rec'},
    'small': {'det': 'PP-OCRv6_small_det', 'rec': 'PP-OCRv6_small_rec'},
    'medium': {'det': 'PP-OCRv6_medium_det', 'rec': 'PP-OCRv6_medium_rec'},
}

# ── CER 计算（编辑距离）──

def edit_distance(s1, s2):
    if len(s1) < len(s2):
        return edit_distance(s2, s1)
    if len(s2) == 0:
        return len(s1)
    prev = list(range(len(s2) + 1))
    for i, c1 in enumerate(s1):
        curr = [i + 1]
        for j, c2 in enumerate(s2):
            ins = prev[j + 1] + 1
            dele = curr[j] + 1
            sub = prev[j] + (0 if c1 == c2 else 1)
            curr.append(min(ins, dele, sub))
        prev = curr
    return prev[-1]

def cer(hypothesis, reference):
    ref = list(reference)
    hyp = list(hypothesis)
    if len(ref) == 0:
        return 1.0 if len(hyp) > 0 else 0.0
    return edit_distance(hyp, ref) / len(ref)

# ── 加载 corpus 期望文本 ──

def load_corpus():
    manifest_path = os.path.join(CORPUS_DIR, 'manifest.json')
    if not os.path.exists(manifest_path):
        print(f'[ERROR] manifest.json 不存在: {manifest_path}', file=sys.stderr)
        sys.exit(1)

    with open(manifest_path, 'r', encoding='utf-8') as f:
        manifest = json.load(f)

    entries = []
    for item in manifest['items']:
        img_path = os.path.join(CORPUS_DIR, item['image'])
        if not os.path.exists(img_path):
            print(f'[WARN] 图片不存在: {img_path}', file=sys.stderr)
            continue
        entries.append({
            'image': img_path,
            'expected_text': item['expected_text'],
            'subset': item['subset'],
            'language': item.get('language', ''),
            'orientation': item.get('orientation', 'horizontal'),
            'width': item.get('width', 0),
            'height': item.get('height', 0),
        })
    return entries

# ── rect 有效性检查 ──

def check_rect_validity(words, image_width, image_height, native_only=False):
    """检查 word rect 的有效性。

    native_only=True 时只检查 native word boxes（_native=True）。
    native_only=False 时检查所有 word boxes。
    """
    if native_only:
        check_words = [w for w in words if w.get('_native', False)]
    else:
        check_words = words

    total = len(check_words)
    valid = 0
    issues = {'empty': 0, 'out_of_bounds': 0, 'negative': 0, 'non_finite': 0}

    for w in check_words:
        rect = w.get('rect', {})
        x = rect.get('x', 0)
        y = rect.get('y', 0)
        rw = rect.get('w', 0)
        rh = rect.get('h', 0)

        # 非有限检查
        if not all(isinstance(v, (int, float)) and abs(v) < 1e9 for v in [x, y, rw, rh]):
            issues['non_finite'] += 1
            continue
        # 空检查
        if rw == 0 and rh == 0:
            issues['empty'] += 1
            continue
        # 负数检查
        if rw < 0 or rh < 0 or x < 0 or y < 0:
            issues['negative'] += 1
            continue
        # 越界检查（5px tolerance）
        if image_width > 0 and image_height > 0:
            if x + rw > image_width + 5 or y + rh > image_height + 5:
                issues['out_of_bounds'] += 1
                continue

        valid += 1

    return valid, total, issues

# ── 执行 OCR 评价 ──

from paddleocr import PaddleOCR

corpus = load_corpus()
print(f'[INFO] Corpus: {len(corpus)} entries')

all_results = {}

for model_name in MODELS:
    print(f'\n=== Model: {model_name} ===', flush=True)

    model_names = MODEL_MAP.get(model_name, MODEL_MAP['small'])

    # PaddleOCR 3.7 API
    engine = PaddleOCR(
        ocr_version='PP-OCRv6',
        text_detection_model_name=model_names['det'],
        text_recognition_model_name=model_names['rec'],
        use_doc_orientation_classify=False,
        use_doc_unwarping=False,
        use_textline_orientation=False,
        return_word_box=True,
        device='cpu',
        enable_mkldnn=False,
    )

    model_results = {'subsets': {}}

    for entry in corpus:
        subset = entry['subset']
        if subset not in model_results['subsets']:
            model_results['subsets'][subset] = {
                'cers': [], 'rect_stats': [],
                'native_word_boxes': 0, 'fallback_word_boxes': 0,
            }

        # PaddleOCR 3.7 predict() 只接受 str(文件路径) 或 numpy.ndarray，不接受 bytes
        import numpy as _np
        from PIL import Image as _PILImage
        import io as _io
        try:
            with open(entry['image'], 'rb') as f:
                png_bytes = f.read()
            _pil_img = _PILImage.open(_io.BytesIO(png_bytes))
            _img_array = _np.array(_pil_img)
        except Exception as e:
            print(f'  [ERROR] image decode {entry["image"]}: {e}', file=sys.stderr)
            model_results['subsets'][subset]['cers'].append(1.0)
            continue

        try:
            result = engine.predict(input=_img_array, return_word_box=True)
        except Exception as e:
            print(f'  [ERROR] {entry["image"]}: {e}', file=sys.stderr)
            model_results['subsets'][subset]['cers'].append(1.0)
            continue

        # 提取识别文本和 word/line 数据
        ocr_text = ''
        lines = []
        words = []
        native_word_boxes = 0
        fallback_word_boxes = 0

        if result:
            for page_result in result:
                try:
                    if hasattr(page_result, 'json'):
                        page_data = page_result.json
                        if isinstance(page_data, str):
                            page_data = json.loads(page_data)
                    elif hasattr(page_result, '__dict__'):
                        page_data = vars(page_result)
                    else:
                        page_data = page_result
                except Exception:
                    page_data = page_result

                # PaddleOCR 3.7 数据嵌套在 "res" 字段下
                if isinstance(page_data, dict) and 'res' in page_data:
                    res = page_data['res']
                elif isinstance(page_data, dict):
                    res = page_data
                else:
                    continue

                if not isinstance(res, dict):
                    continue

                rec_texts = res.get('rec_texts', [])
                rec_scores = res.get('rec_scores', [])
                dt_polys = res.get('dt_polys', [])
                rec_boxes = res.get('rec_boxes', [])
                text_word_boxes = res.get('text_word_boxes', [])
                text_word = res.get('text_word', [])

                line_idx = len(lines)

                for i, text in enumerate(rec_texts):
                    ocr_text += text + '\n'

                    # Line rect from dt_polys or rec_boxes
                    if i < len(dt_polys) and dt_polys[i]:
                        box = dt_polys[i]
                        if box and len(box) >= 4:
                            xs = [p[0] for p in box]
                            ys = [p[1] for p in box]
                            line_rect = {
                                'x': round(min(xs)),
                                'y': round(min(ys)),
                                'w': round(max(xs) - min(xs)),
                                'h': round(max(ys) - min(ys)),
                            }
                        else:
                            line_rect = {'x': 0, 'y': 0, 'w': 0, 'h': 0}
                    elif i < len(rec_boxes) and rec_boxes[i]:
                        box = rec_boxes[i]
                        line_rect = {
                            'x': round(box[0]),
                            'y': round(box[1]),
                            'w': round(box[2] - box[0]),
                            'h': round(box[3] - box[1]),
                        }
                    else:
                        line_rect = {'x': 0, 'y': 0, 'w': 0, 'h': 0}

                    conf = rec_scores[i] if i < len(rec_scores) else 0.0

                    # Word 级数据：text_word_boxes[i] = [[x1,y1,x2,y2], ...], text_word[i] = ["w1", "w2", ...]
                    line_word_indices = []
                    if i < len(text_word_boxes) and i < len(text_word):
                        word_boxes_i = text_word_boxes[i]
                        word_texts_i = text_word[i]
                        for j, w_box in enumerate(word_boxes_i):
                            w_text = word_texts_i[j] if j < len(word_texts_i) else ''
                            if isinstance(w_box, (list, tuple)) and len(w_box) >= 4:
                                word_rect = {
                                    'x': round(w_box[0]),
                                    'y': round(w_box[1]),
                                    'w': round(w_box[2] - w_box[0]),
                                    'h': round(w_box[3] - w_box[1]),
                                }
                            else:
                                word_rect = {'x': 0, 'y': 0, 'w': 0, 'h': 0}

                            words.append({
                                'text': w_text,
                                'rect': word_rect,
                                'line_index': line_idx,
                                '_native': True,
                            })
                            native_word_boxes += 1
                    else:
                        # Fallback: 用文本拆分作为 word（line rect 复制）
                        word_texts = text.split() if text else []
                        if not word_texts and text:
                            word_texts = [text]
                        for wt in word_texts:
                            words.append({
                                'text': wt,
                                'rect': line_rect,
                                'line_index': line_idx,
                                '_native': False,
                            })
                            fallback_word_boxes += 1

                    lines.append({
                        'text': text,
                        'rect': line_rect,
                        'confidence': round(conf, 4),
                    })

        # 计算 CER
        c = cer(ocr_text.strip(), entry['expected_text'].strip())
        model_results['subsets'][subset]['cers'].append(c)

        # rect 有效性（分别统计 native 和 fallback）
        img_w = entry.get('width', 0)
        img_h = entry.get('height', 0)
        if img_w == 0 or img_h == 0:
            try:
                from PIL import Image
                img = Image.open(entry['image'])
                img_w, img_h = img.size
            except Exception:
                img_w, img_h = 99999, 99999

        # native word rect 有效性（资格门只看 native）
        n_valid, n_total, n_issues = check_rect_validity(words, img_w, img_h, native_only=True)
        # fallback word rect 有效性（诊断输出）
        f_valid, f_total, f_issues = check_rect_validity(words, img_w, img_h, native_only=False)

        model_results['subsets'][subset]['rect_stats'].append({
            'native_valid': n_valid,
            'native_total': n_total,
            'native_issues': n_issues,
            'all_valid': f_valid,
            'all_total': f_total,
            'all_issues': f_issues,
            'fallback_count': fallback_word_boxes,
        })
        model_results['subsets'][subset]['native_word_boxes'] += native_word_boxes
        model_results['subsets'][subset]['fallback_word_boxes'] += fallback_word_boxes

        print(f'  [{subset}] {os.path.basename(entry["image"])}: CER={c:.3f} native_words={native_word_boxes} fallback_words={fallback_word_boxes}', flush=True)

    # 计算子集 CER 平均
    for subset, data in model_results['subsets'].items():
        cers = data['cers']
        data['cer_mean'] = round(sum(cers) / len(cers), 4) if cers else 1.0

        # native word rect 有效率（资格门使用此值）
        all_native_valid = sum(r['native_valid'] for r in data['rect_stats'])
        all_native_total = sum(r['native_total'] for r in data['rect_stats'])
        data['native_rect_valid_ratio'] = round(all_native_valid / all_native_total, 4) if all_native_total > 0 else 0.0

        # all word rect 有效率（诊断）
        all_valid = sum(r['all_valid'] for r in data['rect_stats'])
        all_total = sum(r['all_total'] for r in data['rect_stats'])
        data['all_rect_valid_ratio'] = round(all_valid / all_total, 4) if all_total > 0 else 0.0

        # 汇总 issues
        total_issues = {'empty': 0, 'out_of_bounds': 0, 'negative': 0, 'non_finite': 0}
        for r in data['rect_stats']:
            for k, v in r.get('native_issues', {}).items():
                total_issues[k] = total_issues.get(k, 0) + v
        data['rect_issues'] = total_issues

    # 计算加权 CER
    total_weighted = 0
    total_count = 0
    for subset, data in model_results['subsets'].items():
        count = len(data['cers'])
        total_weighted += data['cer_mean'] * count
        total_count += count
    model_results['weighted_cer'] = round(total_weighted / total_count, 4) if total_count > 0 else 1.0

    # native word box 总数
    model_results['total_native_word_boxes'] = sum(d['native_word_boxes'] for d in model_results['subsets'].values())
    model_results['total_fallback_word_boxes'] = sum(d['fallback_word_boxes'] for d in model_results['subsets'].values())

    all_results[model_name] = model_results

# ── 保存结果 ──

out_file = os.path.join(RESULTS_DIR, 'evaluate_results.json')
with open(out_file, 'w', encoding='utf-8') as f:
    json.dump(all_results, f, ensure_ascii=False, indent=2)

print(f'\n=== 评价完成 ===')
print(f'结果文件: {out_file}')
print(f'\n加权 CER:')
for model_name, data in all_results.items():
    print(f'  {model_name}: {data["weighted_cer"]}')
print(f'\nnative word rect 有效率（资格门使用此值）:')
for model_name, data in all_results.items():
    for subset, sd in data['subsets'].items():
        print(f'  {model_name}/{subset}: native_rect_valid={sd["native_rect_valid_ratio"]} native_words={sd["native_word_boxes"]} fallback_words={sd["fallback_word_boxes"]}')
"@

$evalScriptPath = "$ResultsDir\_eval_tmp.py"
$evalScript | Out-File -FilePath $evalScriptPath -Encoding utf8

& $venvPython $evalScriptPath 2>&1 | ForEach-Object { Write-Host $_ }
$exitCode = $LASTEXITCODE

# 清理临时脚本
Remove-Item $evalScriptPath -Force -ErrorAction SilentlyContinue

if ($exitCode -ne 0) {
    throw "评价失败"
}

# ── WinRT 对比 ──

Write-Host ""
Write-Host "=== WinRT 对比 ===" -ForegroundColor Cyan
$winrt = Get-Content $winrtFile | ConvertFrom-Json
$ppocrv = Get-Content "$ResultsDir\evaluate_results.json" | ConvertFrom-Json

foreach ($model in $Models) {
    $ppCER = $ppocrv.$model.weighted_cer
    $winrtCER = $winrt.weighted_cer
    if ($ppCER -and $winrtCER) {
        $change = [math]::Round((($ppCER - $winrtCER) / $winrtCER) * 100, 2)
        $direction = if ($change -lt 0) { "下降（改善）" } else { "上升（退化）" }
        Write-Host "  ${model}: PP-OCRv6 CER=$ppCER vs WinRT CER=$winrtCER (变化: $change% $direction)"

        # 资格门判定：相对下降 >= 10%
        $relativeChange = [math]::Round((($ppCER - $winrtCER) / $winrtCER) * 100, 2)
        if ($relativeChange -le -10) {
            Write-Host "    资格门: PASS (相对下降 >= 10%)" -ForegroundColor Green
        } else {
            Write-Host "    资格门: FAIL (相对下降 < 10%)" -ForegroundColor Red
        }
    }
}

# ── 生成资格门汇总 ──

$gateFile = "$ResultsDir\qualification_gates.json"
$gates = @{
    cer_gate = @{
        description = "加权 CER 相对 WinRT 下降 >= 10%"
        winrt_cer = $winrt.weighted_cer
        models = @{}
    }
    rect_gate = @{
        description = "native word rect 有效率 >= 99%"
        models = @{}
    }
}

foreach ($model in $Models) {
    $ppCER = $ppocrv.$model.weighted_cer
    $winrtCER = $winrt.weighted_cer
    $relativeChange = if ($winrtCER -gt 0) { [math]::Round((($ppCER - $winrtCER) / $winrtCER) * 100, 2) } else { 0 }

    $gates.cer_gate.models.$model = @{
        ppocrv6_cer = $ppCER
        relative_change_pct = $relativeChange
        passed = ($relativeChange -le -10)
    }

    # native rect gate
    $totalNativeValid = 0
    $totalNativeTotal = 0
    foreach ($subset in $ppocrv.$model.subsets.PSObject.Properties.Name) {
        $totalNativeValid += $ppocrv.$model.subsets.$subset.native_rect_valid_ratio
        $totalNativeTotal += 1
    }
    # 加权平均 native rect 有效率
    $allNativeBoxes = 0
    $allValidBoxes = 0
    foreach ($subset in $ppocrv.$model.subsets.PSObject.Properties.Name) {
        $subsetData = $ppocrv.$model.subsets.$subset
        # 使用 rect_stats 中的 native_valid 和 native_total
        foreach ($rs in $subsetData.rect_stats) {
            $allNativeBoxes += $rs.native_total
            $allValidBoxes += $rs.native_valid
        }
    }
    $nativeRectRatio = if ($allNativeBoxes -gt 0) { [math]::Round($allValidBoxes / $allNativeBoxes, 4) } else { 0.0 }

    $gates.rect_gate.models.$model = @{
        native_rect_valid_ratio = $nativeRectRatio
        native_word_boxes = $allNativeBoxes
        passed = ($nativeRectRatio -ge 0.99 -and $allNativeBoxes -gt 0)
    }
}

$gates | ConvertTo-Json -Depth 5 | Out-File -FilePath $gateFile -Encoding utf8
Write-Host ""
Write-Host "资格门汇总: $gateFile" -ForegroundColor Green
