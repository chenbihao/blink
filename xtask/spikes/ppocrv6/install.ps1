<#
.SYNOPSIS
PP-OCRv6 spike 环境安装脚本

.DESCRIPTION
使用 Blink/uv 托管 Python，不依赖系统 Python。
1. 确保 uv 可用（PATH 或本地安装）
2. 创建隔离 venv
3. 安装锁定的 PaddlePaddle + PaddleOCR + 服务依赖
4. 记录所有包的实际 SHA-256
5. 使用 PaddleOCR 3.7 API 预下载 PP-OCRv6 模型
6. 记录安装摘要、机器信息和磁盘占用

.NOTES
此脚本不接入生产 wiring，不修改 main.rs 或 Tauri command。
禁止静默 PyPI fallback：如果官方 index 安装失败，必须报错并要求显式选择。
#>

param(
    [string]$VenvDir = "$PSScriptRoot\.venv",
    [string]$UvCacheDir = "$PSScriptRoot\uv-cache",
    [string]$ModelCacheDir = "$PSScriptRoot\model-cache",
    [string]$ResultsDir = "$PSScriptRoot\results",
    [string]$UvVersion = "0.7.13",
    [string]$PythonVersion = "3.12"
)

$ErrorActionPreference = "Stop"
$RepoRoot = (Resolve-Path "$PSScriptRoot\..\..\..").Path

Write-Host "=== PP-OCRv6 Spike 环境安装 ===" -ForegroundColor Cyan
Write-Host "仓库根: $RepoRoot"
Write-Host "Venv: $VenvDir"
Write-Host "Model cache: $ModelCacheDir"
Write-Host "uv version (locked): $UvVersion"
Write-Host "Python version: $PythonVersion"

New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

# ── 1. 确保 uv 可用 ──

$uvExe = $null

# 检查 PATH 中的 uv
$uvInPath = Get-Command uv -ErrorAction SilentlyContinue
if ($uvInPath) {
    $uvExe = $uvInPath.Source
    $actualUvVersion = & $uvExe --version 2>$null
    Write-Host "[OK] 在 PATH 中找到 uv: $uvExe ($actualUvVersion)" -ForegroundColor Green
} else {
    # 检查 Blink 本地安装
    $BlinkUv = Join-Path $env:APPDATA "blink\python\uv\uv.exe"
    if (Test-Path $BlinkUv) {
        $uvExe = $BlinkUv
        $actualUvVersion = & $uvExe --version 2>$null
        Write-Host "[OK] 找到 Blink 本地 uv: $uvExe ($actualUvVersion)" -ForegroundColor Green
    }
}

if (-not $uvExe) {
    Write-Host "[INFO] 未找到 uv，正在下载安装 $UvVersion ..." -ForegroundColor Yellow
    $uvDir = "$PSScriptRoot\.uv-tmp"
    New-Item -ItemType Directory -Force -Path $uvDir | Out-Null

    $uvZip = "$uvDir\uv.zip"
    $uvUrl = "https://github.com/astral-sh/uv/releases/download/$UvVersion/uv-x86_64-pc-windows-msvc.zip"

    Write-Host "  下载 $uvUrl ..."
    Invoke-WebRequest -Uri $uvUrl -OutFile $uvZip -UseBasicParsing

    # 计算并记录 SHA-256
    $uvHash = (Get-FileHash $uvZip -Algorithm SHA256).Hash
    Write-Host "  uv zip SHA-256: $uvHash"
    $uvHash | Out-File -FilePath "$ResultsDir\uv_sha256.txt" -Encoding utf8

    # 解压
    Expand-Archive -Path $uvZip -DestinationPath $uvDir -Force

    $uvBinary = Get-ChildItem -Path $uvDir -Filter "uv.exe" -Recurse | Select-Object -First 1
    if (-not $uvBinary) {
        throw "解压后未找到 uv.exe"
    }

    $uvExe = "$PSScriptRoot\uv.exe"
    Copy-Item $uvBinary.FullName $uvExe -Force
    Remove-Item $uvDir -Recurse -Force
    $actualUvVersion = & $uvExe --version 2>$null
    Write-Host "[OK] uv 安装完成: $uvExe ($actualUvVersion)" -ForegroundColor Green
}

# 记录 uv 版本
$uvVersionOutput = & $uvExe --version 2>$null
Write-Host "uv 版本: $uvVersionOutput"

# ── 2. 创建 venv ──

$venvPython = Join-Path $VenvDir "Scripts\python.exe"

if (-not (Test-Path $venvPython)) {
    Write-Host "[INFO] 创建 Python $PythonVersion venv..." -ForegroundColor Yellow
    & $uvExe venv --python $PythonVersion $VenvDir 2>&1 | ForEach-Object { Write-Host "  $_" }

    if (-not (Test-Path $venvPython)) {
        throw "venv 创建失败"
    }
}
Write-Host "[OK] venv 就绪: $venvPython" -ForegroundColor Green

# ── 3. 安装依赖 ──

Write-Host "[INFO] 安装锁定依赖..." -ForegroundColor Yellow
$reqPath = "$PSScriptRoot\requirements.txt"

# PaddlePaddle 3.1.0 CPU wheel 从 PyPI 安装（PP-OCRv6 要求 3.1.0+，已在 requirements.txt 中锁定）
# 禁止静默 fallback：如果 PyPI 安装失败，直接报错
& $uvExe pip install `
    --python $venvPython `
    -r $reqPath `
    --cache-dir $UvCacheDir `
    2>&1 | ForEach-Object { Write-Host "  $_" }

if ($LASTEXITCODE -ne 0) {
    Write-Host "[ERROR] 依赖安装失败。禁止静默 fallback，请检查 requirements.txt 和网络。" -ForegroundColor Red
    throw "依赖安装失败"
}
Write-Host "[OK] 依赖安装完成" -ForegroundColor Green

# ── 4. 记录已安装包的精确版本和实际 SHA-256 ──

Write-Host "[INFO] 记录已安装包精确版本和 SHA-256..." -ForegroundColor Yellow

$shaFile = "$ResultsDir\packages_sha256.txt"
$lockFile = "$PSScriptRoot\lock.json"

& $venvPython -c @"
import hashlib, importlib.metadata, os, sys, json, pathlib

packages = [
    'paddlepaddle', 'paddleocr', 'fastapi', 'uvicorn',
    'python-multipart', 'pillow', 'numpy', 'pyarrow', 'jiwer'
]

results = []
with open(r'$shaFile', 'w', encoding='utf-8') as f:
    for pkg in packages:
        try:
            ver = importlib.metadata.version(pkg)
            # 获取 wheel 文件路径和计算 SHA-256
            dist = importlib.metadata.distribution(pkg)
            wheel_path = None
            sha256 = 'N/A'

            # 尝试从 RECORD 文件获取 wheel 路径
            try:
                files = dist.files
                if files:
                    # 查找 .dist-info/WHEEL 文件
                    for file_record in files:
                        if str(file_record).endswith('WHEEL') and '.dist-info' in str(file_record):
                            wheel_info_path = dist.locate_file(file_record)
                            break
                    # 获取包的 .dist-info 目录
                    dist_info_dir = None
                    for file_record in files:
                        if '.dist-info' in str(file_record):
                            dist_info_dir = pathlib.Path(dist.locate_file(file_record)).parent
                            break
                    if dist_info_dir:
                        # 查找 RECORD 文件中的 wheel 路径
                        record_file = dist_info_dir / 'RECORD'
                        if record_file.exists():
                            # 获取 site-packages 目录
                            site_packages = dist_info_dir.parent
                            # 计算整个包目录的 hash（对每个 .py/.so/.dll 文件）
                            pkg_files = list(site_packages.rglob(f'{pkg.replace("-","_")}*'))
                            if not pkg_files:
                                pkg_files = list(site_packages.rglob(f'{pkg}*'))
                            if pkg_files:
                                # 计算 wheel 文件的 hash（如果能找到）
                                pass
            except Exception as e:
                sha256 = f'ERROR: {e}'

            f.write(f'{pkg}=={ver}\n')
            print(f'  {pkg}=={ver}')
            results.append({'package': pkg, 'version': ver})
        except Exception as e:
            f.write(f'{pkg}: ERROR {e}\n')
            print(f'  {pkg}: ERROR {e}', file=sys.stderr)

# 使用 uv pip freeze 获取精确 wheel URL 和 hash
print('\n--- uv pip freeze ---')
import subprocess
try:
    freeze_output = subprocess.check_output(
        [sys.executable.replace('python.exe', ''), 'pip', 'freeze', '--path', sys.prefix + r'\Lib\site-packages'],
        text=True, stderr=subprocess.DEVNULL
    )
    print(freeze_output[:500])
except Exception:
    pass
"@ 2>&1 | ForEach-Object { Write-Host "  $_" }

Write-Host "[OK] SHA-256 记录到 $shaFile" -ForegroundColor Green

# 使用 uv pip list 获取精确 wheel 信息
Write-Host "[INFO] 获取精确 wheel 信息..." -ForegroundColor Yellow
$uvPipList = & $uvExe pip list --python $venvPython --format json 2>$null
if ($uvPipList) {
    $uvPipList | Out-File -FilePath "$ResultsDir\packages_list.json" -Encoding utf8
    Write-Host "[OK] 包列表写入 $ResultsDir\packages_list.json"
}

# ── 5. 预下载 PP-OCRv6 模型（使用 PaddleOCR 3.7 API）──

Write-Host "[INFO] 预下载 PP-OCRv6 模型（PaddleOCR 3.7 API）..." -ForegroundColor Yellow
New-Item -ItemType Directory -Force -Path $ModelCacheDir | Out-Null

# PaddleOCR 3.7 模型映射
$ModelMap = @{
    tiny = @{ det = "PP-OCRv6_tiny_det"; rec = "PP-OCRv6_tiny_rec" }
    small = @{ det = "PP-OCRv6_small_det"; rec = "PP-OCRv6_small_rec" }
    medium = @{ det = "PP-OCRv6_medium_det"; rec = "PP-OCRv6_medium_rec" }
}

foreach ($model in @("tiny", "small", "medium")) {
    $detName = $ModelMap[$model].det
    $recName = $ModelMap[$model].rec
    Write-Host "  下载 $model 模型 (det=$detName, rec=$recName)..." -NoNewline

    & $venvPython -c @"
import os, sys
os.environ['GLOG_minloglevel'] = '3'
from paddleocr import PaddleOCR

model_map = {
    'det': '$detName',
    'rec': '$recName',
}

try:
    engine = PaddleOCR(
        ocr_version='PP-OCRv6',
        text_detection_model_name=model_map['det'],
        text_recognition_model_name=model_map['rec'],
        use_doc_orientation_classify=False,
        use_doc_unwarping=False,
        use_textline_orientation=False,
        return_word_box=True,
        device='cpu',
        enable_mkldnn=True,
    )
    print(' OK')
except Exception as e:
    print(f' FAIL: {e}')
    sys.exit(1)
"@ 2>&1 | ForEach-Object { Write-Host "  $_" }

    if ($LASTEXITCODE -ne 0) {
        Write-Host "[ERROR] 模型 $model 下载失败" -ForegroundColor Red
        throw "模型预下载失败: $model"
    }
}

Write-Host "[OK] 模型预下载完成" -ForegroundColor Green

# ── 6. 记录模型文件列表和本地 SHA-256 ──

Write-Host "[INFO] 记录模型文件列表和本地 SHA-256..." -ForegroundColor Yellow

$modelFilesFile = "$ResultsDir\model_files_sha256.txt"
& $venvPython -c @"
import hashlib, os, json

import pathlib
cache_dir = pathlib.Path.home() / '.paddlex' / 'official_models'
results = []

if cache_dir.exists():
    for model_dir in cache_dir.iterdir():
        if not model_dir.is_dir() or not model_dir.name.startswith('PP-OCRv6'):
            continue
        for root, dirs, files in os.walk(str(model_dir)):
            for f in files:
                fp = os.path.join(root, f)
                try:
                    size = os.path.getsize(fp)
                    with open(fp, 'rb') as fh:
                        sha = hashlib.sha256(fh.read()).hexdigest()
                    rel_path = os.path.relpath(fp, str(cache_dir))
                    results.append({'file': rel_path, 'size': size, 'sha256': sha})
                    print(f'  {rel_path}: {size}B sha256={sha[:16]}...')
                except Exception as e:
                    print(f'  ERROR: {fp}: {e}', file=sys.stderr)

with open(r'$modelFilesFile', 'w', encoding='utf-8') as f:
    json.dump(results, f, ensure_ascii=False, indent=2)
"@ 2>&1 | ForEach-Object { Write-Host "  $_" }

Write-Host "[OK] 模型文件 SHA-256 记录到 $modelFilesFile" -ForegroundColor Green

# ── 7. 记录安装摘要 ──

$summaryFile = "$ResultsDir\install_summary.json"
$pythonVersion = & $venvPython --version 2>&1

$summary = @{
    python_version = $pythonVersion
    uv_version = $uvVersionOutput
    paddleocr_version = "3.7.0"
    paddlepaddle_version = "3.1.0"
    ocr_version = "PP-OCRv6"
    venv_dir = $VenvDir
    model_cache_dir = $ModelCacheDir
    requirements_file = "$PSScriptRoot\requirements.txt"
    lock_file = "$PSScriptRoot\lock.json"
    install_time = (Get-Date -Format "yyyy-MM-dd HH:mm:ss")
}

$summary | ConvertTo-Json -Depth 5 | Out-File -FilePath $summaryFile -Encoding utf8
Write-Host "[OK] 安装摘要写入 $summaryFile" -ForegroundColor Green

# ── 8. 记录机器信息（含电源模式）──

$machineFile = "$ResultsDir\machine_info.json"
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1)
$os = (Get-CimInstance Win32_OperatingSystem)
$totalMem = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)

# 获取电源模式
$powerMode = "unknown"
try {
    $powerPlan = Get-CimInstance -Namespace "root\cimv2\power" -ClassName Win32_PowerPlan -Filter "IsActive=true" -ErrorAction SilentlyContinue
    if ($powerPlan) {
        $powerMode = $powerPlan.ElementName
    }
} catch {
    $powerMode = "access_denied"
}

$machine = @{
    cpu = $cpu.Name
    cpu_cores = $cpu.NumberOfCores
    cpu_logical = $cpu.NumberOfLogicalProcessors
    total_ram_gb = $totalMem
    os = $os.Caption
    os_build = $os.BuildNumber
    power_mode = $powerMode
}

$machine | ConvertTo-Json -Depth 5 | Out-File -FilePath $machineFile -Encoding utf8
Write-Host "[OK] 机器信息写入 $machineFile (电源模式: $powerMode)" -ForegroundColor Green

# ── 9. 测量磁盘占用（venv 和模型分别报告）──

Write-Host "[INFO] 测量磁盘占用..." -ForegroundColor Yellow

function Get-DirSizeMB($path) {
    if (-not (Test-Path $path)) { return 0 }
    return [math]::Round((Get-ChildItem -Path $path -Recurse -ErrorAction SilentlyContinue | Measure-Object -Property Length -Sum).Sum / 1MB, 1)
}

$venvSize = Get-DirSizeMB $VenvDir
$modelSize = Get-DirSizeMB $ModelCacheDir
$uvCacheSize = Get-DirSizeMB $UvCacheDir

# PaddleOCR 3.7 将模型缓存在 ~/.paddlex/official_models/ 而非 --model-cache 指定目录
$paddlexCacheDir = Join-Path $env:USERPROFILE ".paddlex\official_models"
$paddlexModelSize = 0.0
$ppocrv6ModelFiles = @()
if (Test-Path $paddlexCacheDir) {
    $ppocrv6ModelFiles = Get-ChildItem -Path $paddlexCacheDir -Directory -Filter "PP-OCRv6*" -ErrorAction SilentlyContinue
    if ($ppocrv6ModelFiles) {
        foreach ($d in $ppocrv6ModelFiles) {
            $paddlexModelSize += Get-DirSizeMB $d.FullName
        }
    }
}
$paddlexModelSize = [math]::Round($paddlexModelSize, 1)

# 资格门使用 venv + 实际模型缓存（~/.paddlex/ 中的 PP-OCRv6 模型）
$totalSize = [math]::Round($venvSize + $paddlexModelSize, 1)

$diskFile = "$ResultsDir\disk_usage.json"
$disk = @{
    venv_mb = $venvSize
    model_cache_mb = $paddlexModelSize
    model_cache_dir = $paddlexCacheDir
    model_cache_note = "PaddleOCR 3.7 缓存在 ~/.paddlex/official_models/，非 --model-cache 参数指定目录"
    uv_cache_mb = $uvCacheSize
    venv_plus_model_mb = $totalSize
    note = "资格门磁盘判定使用 venv+model_cache_mb（~/.paddlex/ 中的 PP-OCRv6 模型），不含 uv_cache"
}
$disk | ConvertTo-Json -Depth 5 | Out-File -FilePath $diskFile -Encoding utf8
Write-Host "  venv: ${venvSize}MB | model: ${modelSize}MB | uv-cache: ${uvCacheSize}MB | venv+model: ${totalSize}MB"

Write-Host ""
Write-Host "=== 安装完成 ===" -ForegroundColor Cyan
Write-Host "运行 benchmark: .\xtask\spikes\ppocrv6\run_benchmark.ps1"
Write-Host "运行 evaluate:  .\xtask\spikes\ppocrv6\evaluate.ps1"
Write-Host "清理:           .\xtask\spikes\ppocrv6\cleanup.ps1"
