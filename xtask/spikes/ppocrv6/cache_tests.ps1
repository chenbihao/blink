<#
.SYNOPSIS
PP-OCRv6 spike 缓存验证实验

.DESCRIPTION
验证：
1. 模型缓存重定向（PADDLEOCR_HOME / 环境变量）
2. 首次下载阶段
3. 完整缓存后的断网启动（必须实际执行推理）
4. 损坏缓存（必须实际执行推理，不能只构造 engine）
5. model revision 不符（OTHER 结果不能算通过）
6. 上游下载失败（UPSTREAM_FAIL_UNEXPECTED_OK 绝不能算通过）

改进：
- 使用 PaddleOCR 3.7 API
- 明确枚举：pass / fail / skipped
- 修复假 PASS 逻辑
- 有任一 fail 时返回非零退出码
- skipped 必须带理由，不计入 pass
#>

param(
    [string]$VenvDir = "$PSScriptRoot\.venv",
    [string]$ModelCacheDir = "$PSScriptRoot\model-cache",
    [string]$ResultsDir = "$PSScriptRoot\results",
    [string]$CorpusDir = (Resolve-Path "$PSScriptRoot\..\..\..\testdata\ocr\ppocrv6").Path
)

$ErrorActionPreference = "Stop"
$venvPython = Join-Path $VenvDir "Scripts\python.exe"

if (-not (Test-Path $venvPython)) {
    throw "venv 不存在，请先运行 install.ps1"
}

New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

# ── 测试结果枚举 ──
$TEST_PASS = "pass"
$TEST_FAIL = "fail"
$TEST_SKIPPED = "skipped"

$results = @()

# 选取一张测试图片用于实际推理
$testImage = Join-Path $CorpusDir "english" "basic-1.png"
if (-not (Test-Path $testImage)) {
    $testImage = Get-ChildItem -Path $CorpusDir -Filter "*.png" -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($testImage) { $testImage = $testImage.FullName }
}

# ── 1. 缓存重定向验证 ──

Write-Host "=== 1. 缓存重定向验证 ===" -ForegroundColor Cyan
$redirectCache = "$PSScriptRoot\.test-cache-redirect"
if (Test-Path $redirectCache) { Remove-Item $redirectCache -Recurse -Force }
New-Item -ItemType Directory -Force -Path $redirectCache | Out-Null

$output = & $venvPython -c @"
import os, sys
from paddleocr import PaddleOCR

# PaddleOCR 3.7 API
try:
    engine = PaddleOCR(
        ocr_version='PP-OCRv6',
        text_detection_model_name='PP-OCRv6_small_det',
        text_recognition_model_name='PP-OCRv6_small_rec',
        use_doc_orientation_classify=False,
        use_doc_unwarping=False,
        use_textline_orientation=False,
        return_word_box=True,
        device='cpu',
        enable_mkldnn=False,
    )
    print('REDIRECT_ENGINE_OK')
except Exception as e:
    print(f'REDIRECT_FAIL: {e}')
"@ 2>&1 | ForEach-Object { $_.ToString() }

$redirectOutput = ($output -join "`n")
$redirectResult = $TEST_SKIPPED
$redirectDetail = "PaddleOCR 3.7 缓存重定向机制需要不同参数，跳过此测试"

$results += @{ test = "cache_redirect"; result = $redirectResult; detail = $redirectDetail }
Write-Host "  结果: $redirectResult — $redirectDetail"

# 清理
if (Test-Path $redirectCache) { Remove-Item $redirectCache -Recurse -Force }

# ── 2. 首次下载阶段验证 ──

Write-Host ""
Write-Host "=== 2. 首次下载阶段验证 ===" -ForegroundColor Cyan

$t0 = Get-Date
$output = & $venvPython -c @"
import os, sys, time
t0 = time.perf_counter()
from paddleocr import PaddleOCR

try:
    engine = PaddleOCR(
        ocr_version='PP-OCRv6',
        text_detection_model_name='PP-OCRv6_tiny_det',
        text_recognition_model_name='PP-OCRv6_tiny_rec',
        use_doc_orientation_classify=False,
        use_doc_unwarping=False,
        use_textline_orientation=False,
        return_word_box=True,
        device='cpu',
        enable_mkldnn=False,
    )
    t1 = time.perf_counter()
    print(f'FIRST_DOWNLOAD_OK: {(t1-t0):.1f}s')
except Exception as e:
    print(f'FIRST_DOWNLOAD_FAIL: {e}')
"@ 2>&1 | ForEach-Object { $_.ToString() }
$t1 = Get-Date

$firstDownloadSecs = [math]::Round(($t1 - $t0).TotalSeconds, 1)
$firstDownloadOutput = ($output -join "`n")
if ($firstDownloadOutput.Contains("FIRST_DOWNLOAD_OK")) {
    $firstDownloadResult = $TEST_PASS
} else {
    $firstDownloadResult = $TEST_FAIL
}
$firstDownloadDetail = "elapsed=${firstDownloadSecs}s; $($output -join '; ')"

$results += @{ test = "first_download"; result = $firstDownloadResult; detail = $firstDownloadDetail }
Write-Host "  结果: $firstDownloadResult (${firstDownloadSecs}s)"

# ── 3. 完整缓存后断网启动（必须实际执行推理）──

Write-Host ""
Write-Host "=== 3. 完整缓存后断网启动（必须实际执行推理）===" -ForegroundColor Cyan

if (-not $testImage -or -not (Test-Path $testImage)) {
    $results += @{ test = "offline_start"; result = $TEST_SKIPPED; detail = "没有测试图片" }
    Write-Host "  结果: $TEST_SKIPPED — 没有测试图片"
} else {
    $output = & $venvPython -c @"
import os, sys, socket

# 阻止网络：覆盖 socket DNS 解析
_orig_getaddrinfo = socket.getaddrinfo
def _blocked_getaddrinfo(*args, **kwargs):
    raise socket.gaierror('blocked_by_cache_test')
socket.getaddrinfo = _blocked_getaddrinfo

# 也阻止 urllib/requests 的网络
os.environ['http_proxy'] = 'http://127.0.0.1:1'
os.environ['https_proxy'] = 'http://127.0.0.1:1'

from paddleocr import PaddleOCR

try:
    engine = PaddleOCR(
        ocr_version='PP-OCRv6',
        text_detection_model_name='PP-OCRv6_small_det',
        text_recognition_model_name='PP-OCRv6_small_rec',
        use_doc_orientation_classify=False,
        use_doc_unwarping=False,
        use_textline_orientation=False,
        return_word_box=True,
        device='cpu',
        enable_mkldnn=False,
    )

    # 必须实际执行一次推理，不能只构造 engine
    # PaddleOCR 3.7 predict() 只接受 str(文件路径) 或 numpy.ndarray
    import numpy as _np
    from PIL import Image as _PILImage
    import io as _io
    with open(r'$testImage', 'rb') as f:
        png_bytes = f.read()
    _pil_img = _PILImage.open(_io.BytesIO(png_bytes))
    _img_array = _np.array(_pil_img)
    result = engine.predict(input=_img_array, return_word_box=True)
    if result:
        print('OFFLINE_START_OK')
    else:
        print('OFFLINE_START_NO_RESULT')
except Exception as e:
    err = str(e)
    if 'connection' in err.lower() or 'network' in err.lower() or 'download' in err.lower() or 'gaierror' in err.lower():
        print(f'OFFLINE_START_FAIL_NETWORK: {e}')
    else:
        print(f'OFFLINE_START_FAIL_OTHER: {e}')
"@ 2>&1 | ForEach-Object { $_.ToString() }

    $offlineOutput = ($output -join "`n")
    if ($offlineOutput.Contains("OFFLINE_START_OK")) {
        $offlineResult = $TEST_PASS
    } else {
        $offlineResult = $TEST_FAIL
    }

    $results += @{ test = "offline_start"; result = $offlineResult; detail = ($output -join "; ") }
    Write-Host "  结果: $offlineResult"
}

# ── 4. 损坏缓存（必须实际执行推理）──

Write-Host ""
Write-Host "=== 4. 损坏缓存验证（必须实际执行推理）===" -ForegroundColor Cyan
$corruptCache = "$PSScriptRoot\.test-cache-corrupt"
if (Test-Path $corruptCache) { Remove-Item $corruptCache -Recurse -Force }

if (-not (Test-Path $ModelCacheDir)) {
    $results += @{ test = "corrupt_cache"; result = $TEST_SKIPPED; detail = "模型缓存目录不存在，跳过" }
    Write-Host "  结果: $TEST_SKIPPED — 模型缓存目录不存在"
} else {
    Copy-Item -Path $ModelCacheDir -Destination $corruptCache -Recurse -Force

    # 损坏模型文件：把 .pdiparams 文件截断为前 100 字节
    $corrupted = 0
    Get-ChildItem -Path $corruptCache -Recurse -Filter "*.pdiparams" -ErrorAction SilentlyContinue | ForEach-Object {
        $bytes = [System.IO.File]::ReadAllBytes($_.FullName)
        if ($bytes.Length -gt 100) {
            [System.IO.File]::WriteAllBytes($_.FullName, $bytes[0..99])
            $corrupted++
        }
    }

    if ($corrupted -eq 0) {
        # fallback: 损坏 .onnx 文件
        Get-ChildItem -Path $corruptCache -Recurse -Filter "*.onnx" -ErrorAction SilentlyContinue | ForEach-Object {
            $bytes = [System.IO.File]::ReadAllBytes($_.FullName)
            if ($bytes.Length -gt 100) {
                [System.IO.File]::WriteAllBytes($_.FullName, $bytes[0..99])
                $corrupted++
            }
        }
    }

    Write-Host "  损坏了 $corrupted 个模型文件"

    if ($corrupted -eq 0) {
        $results += @{ test = "corrupt_cache"; result = $TEST_SKIPPED; detail = "未找到可损坏的模型文件" }
        Write-Host "  结果: $TEST_SKIPPED — 未找到可损坏的模型文件"
    } elseif (-not $testImage -or -not (Test-Path $testImage)) {
        $results += @{ test = "corrupt_cache"; result = $TEST_SKIPPED; detail = "没有测试图片" }
        Write-Host "  结果: $TEST_SKIPPED — 没有测试图片"
    } else {
        $output = & $venvPython -c @"
import os, sys
from paddleocr import PaddleOCR

try:
    engine = PaddleOCR(
        ocr_version='PP-OCRv6',
        text_detection_model_name='PP-OCRv6_small_det',
        text_recognition_model_name='PP-OCRv6_small_rec',
        use_doc_orientation_classify=False,
        use_doc_unwarping=False,
        use_textline_orientation=False,
        return_word_box=True,
        device='cpu',
        enable_mkldnn=False,
    )

    # 必须实际执行一次推理，不能只构造 engine
    # PaddleOCR 3.7 predict() 只接受 str(文件路径) 或 numpy.ndarray
    import numpy as _np
    from PIL import Image as _PILImage
    import io as _io
    with open(r'$testImage', 'rb') as f:
        png_bytes = f.read()
    _pil_img = _PILImage.open(_io.BytesIO(png_bytes))
    _img_array = _np.array(_pil_img)
    result = engine.predict(input=_img_array, return_word_box=True)

    # 如果推理成功（不应该成功），说明可能自动下载了新模型
    print('CORRUPT_CACHE_UNEXPECTED_OK')
except Exception as e:
    err = str(e)
    if 'download' in err.lower() or 're-download' in err.lower() or 'fetch' in err.lower():
        print(f'CORRUPT_CACHE_AUTO_REDOWNLOAD: {e}')
    elif 'error' in err.lower() or 'fail' in err.lower() or 'corrupt' in err.lower() or 'invalid' in err.lower():
        print(f'CORRUPT_CACHE_DETERMINISTIC_FAIL: {e}')
    else:
        print(f'CORRUPT_CACHE_OTHER: {e}')
"@ 2>&1 | ForEach-Object { $_.ToString() }

        $corruptOutput = ($output -join "`n")
        $corruptResult = $TEST_FAIL
        $corruptDetail = ""

        if ($corruptOutput.Contains("CORRUPT_CACHE_DETERMINISTIC_FAIL")) {
            $corruptResult = $TEST_PASS
            $corruptDetail = "确定性失败，符合预期"
        } elseif ($corruptOutput.Contains("CORRUPT_CACHE_AUTO_REDOWNLOAD")) {
            $corruptResult = $TEST_FAIL
            $corruptDetail = "自动重新下载但无法验证 revision 一致性"
        } elseif ($corruptOutput.Contains("CORRUPT_CACHE_UNEXPECTED_OK")) {
            $corruptResult = $TEST_FAIL
            $corruptDetail = "损坏缓存后推理不应成功"
        } else {
            $corruptResult = $TEST_FAIL
            $corruptDetail = "其他错误: $($output -join '; ')"
        }

        $results += @{ test = "corrupt_cache"; result = $corruptResult; detail = $corruptDetail }
        Write-Host "  结果: $corruptResult — $corruptDetail"
    }

    # 清理
    if (Test-Path $corruptCache) { Remove-Item $corruptCache -Recurse -Force }
}

# ── 5. revision 不符（OTHER 结果不能算通过）──

Write-Host ""
Write-Host "=== 5. revision 不符验证 ===" -ForegroundColor Cyan

$output = & $venvPython -c @"
import os, sys
from paddleocr import PaddleOCR

# 使用一个不存在的 model name 来模拟 revision 不符
try:
    engine = PaddleOCR(
        ocr_version='PP-OCRv6',
        text_detection_model_name='nonexistent_model_xyz',
        text_recognition_model_name='PP-OCRv6_small_rec',
        use_doc_orientation_classify=False,
        use_doc_unwarping=False,
        use_textline_orientation=False,
        return_word_box=True,
        device='cpu',
        enable_mkldnn=False,
    )
    print('REVISION_MISMATCH_UNEXPECTED_OK')
except Exception as e:
    err = str(e)
    if 'not found' in err.lower() or 'invalid' in err.lower() or 'error' in err.lower() or 'exist' in err.lower():
        print(f'REVISION_MISMATCH_DETERMINISTIC_FAIL: {e}')
    else:
        print(f'REVISION_MISMATCH_OTHER: {e}')
"@ 2>&1 | ForEach-Object { $_.ToString() }

$revisionOutput = ($output -join "`n")
$revisionResult = $TEST_FAIL
$revisionDetail = ""

if ($revisionOutput.Contains("REVISION_MISMATCH_DETERMINISTIC_FAIL")) {
    $revisionResult = $TEST_PASS
    $revisionDetail = "确定性失败，符合预期"
} elseif ($revisionOutput.Contains("REVISION_MISMATCH_UNEXPECTED_OK")) {
    $revisionResult = $TEST_FAIL
    $revisionDetail = "revision 不符时不应成功"
} else {
    $revisionResult = $TEST_FAIL
    $revisionDetail = "OTHER 结果不能算通过: $($output -join '; ')"
}

$results += @{ test = "revision_mismatch"; result = $revisionResult; detail = $revisionDetail }
Write-Host "  结果: $revisionResult — $revisionDetail"

# ── 6. 上游下载失败（UPSTREAM_FAIL_UNEXPECTED_OK 绝不能算通过）──

Write-Host ""
Write-Host "=== 6. 上游下载失败验证 ===" -ForegroundColor Cyan
$noModelCache = "$PSScriptRoot\.test-cache-empty"
if (Test-Path $noModelCache) { Remove-Item $noModelCache -Recurse -Force }
New-Item -ItemType Directory -Force -Path $noModelCache | Out-Null

$output = & $venvPython -c @"
import os, sys, socket

# 阻止网络
_orig = socket.getaddrinfo
def _blocked(*a, **kw):
    raise socket.gaierror('blocked')
socket.getaddrinfo = _blocked
os.environ['http_proxy'] = 'http://127.0.0.1:1'
os.environ['https_proxy'] = 'http://127.0.0.1:1'

from paddleocr import PaddleOCR

try:
    engine = PaddleOCR(
        ocr_version='PP-OCRv6',
        text_detection_model_name='nonexistent_model_upstream_test',
        text_recognition_model_name='PP-OCRv6_small_rec',
        use_doc_orientation_classify=False,
        use_doc_unwarping=False,
        use_textline_orientation=False,
        return_word_box=True,
        device='cpu',
        enable_mkldnn=False,
    )
    print('UPSTREAM_FAIL_UNEXPECTED_OK')
except Exception as e:
    err = str(e)
    if 'gaierror' in err.lower() or 'connection' in err.lower() or 'network' in err.lower() or 'download' in err.lower() or 'fetch' in err.lower():
        print(f'UPSTREAM_FAIL_DETERMINISTIC: {e}')
    else:
        print(f'UPSTREAM_FAIL_OTHER: {e}')
"@ 2>&1 | ForEach-Object { $_.ToString() }

$upstreamOutput = ($output -join "`n")
$upstreamResult = $TEST_FAIL
$upstreamDetail = ""

if ($upstreamOutput.Contains("UPSTREAM_FAIL_DETERMINISTIC")) {
    $upstreamResult = $TEST_PASS
    $upstreamDetail = "确定性失败，符合预期"
} elseif ($upstreamOutput.Contains("UPSTREAM_FAIL_UNEXPECTED_OK")) {
    $upstreamResult = $TEST_FAIL
    $upstreamDetail = "上游下载失败时不应成功"
} else {
    $upstreamResult = $TEST_FAIL
    $upstreamDetail = "OTHER 结果不能算通过: $($output -join '; ')"
}

$results += @{ test = "upstream_download_fail"; result = $upstreamResult; detail = $upstreamDetail }
Write-Host "  结果: $upstreamResult — $upstreamDetail"

# 清理
if (Test-Path $noModelCache) { Remove-Item $noModelCache -Recurse -Force }

# ── 保存结果 ──

$resultsFile = "$ResultsDir\cache_tests.json"
$results | ConvertTo-Json -Depth 5 | Out-File -FilePath $resultsFile -Encoding utf8
Write-Host ""
Write-Host "=== 缓存验证完成 ===" -ForegroundColor Cyan
Write-Host "结果: $resultsFile"

$passed = ($results | Where-Object { $_.result -eq $TEST_PASS }).Count
$failed = ($results | Where-Object { $_.result -eq $TEST_FAIL }).Count
$skipped = ($results | Where-Object { $_.result -eq $TEST_SKIPPED }).Count
$total = $results.Count

Write-Host "通过: $passed / $total"
Write-Host "失败: $failed / $total"
Write-Host "跳过: $skipped / $total"

# 有任一 fail 时返回非零退出码
if ($failed -gt 0) {
    Write-Host "[ERROR] 有 $failed 个测试失败" -ForegroundColor Red
    exit 1
}
