<#
.SYNOPSIS
PP-OCRv6 spike 清理脚本

.DESCRIPTION
删除 spike 产生的 venv、模型缓存、uv cache 和临时输出。
不会删除 spike 脚本本身、lock.json 和 golden corpus。

.PARAMETER All
清理所有临时产物，包括 results/。
#>

param(
    [switch]$All
)

$ErrorActionPreference = "Continue"

Write-Host "=== PP-OCRv6 Spike 清理 ===" -ForegroundColor Cyan

$dirs = @(
    "$PSScriptRoot\.venv",
    "$PSScriptRoot\model-cache",
    "$PSScriptRoot\uv-cache",
    "$PSScriptRoot\.uv-tmp",
    "$PSScriptRoot\.test-cache-redirect",
    "$PSScriptRoot\.test-cache-fresh",
    "$PSScriptRoot\.test-cache-corrupt",
    "$PSScriptRoot\.test-cache-empty"
)

foreach ($dir in $dirs) {
    if (Test-Path $dir) {
        $size = [math]::Round((Get-ChildItem -Path $dir -Recurse -ErrorAction SilentlyContinue | Measure-Object -Property Length -Sum).Sum / 1MB, 1)
        Write-Host "  删除 $dir (${size}MB)..." -NoNewline
        Remove-Item -Path $dir -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path $dir) {
            Write-Host " [部分失败]" -ForegroundColor Yellow
        } else {
            Write-Host " OK" -ForegroundColor Green
        }
    }
}

# 删除 uv.exe（如果由 install.ps1 下载的）
$uvExe = "$PSScriptRoot\uv.exe"
if (Test-Path $uvExe) {
    Remove-Item $uvExe -Force -ErrorAction SilentlyContinue
    Write-Host "  删除 uv.exe OK" -ForegroundColor Green
}

# 删除 results/（仅 --All 时）
if ($All) {
    $resultsDir = "$PSScriptRoot\results"
    if (Test-Path $resultsDir) {
        Write-Host "  删除 results/ ..." -NoNewline
        Remove-Item $resultsDir -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host " OK" -ForegroundColor Green
    }
}

# 验证无残留 Python 进程
$pyProcs = Get-Process -Name "python*" -ErrorAction SilentlyContinue | Where-Object {
    $_.Path -like "*spikes\ppocrv6*" -or
    $_.CommandLine -like "*ppocrv6*"
}

if ($pyProcs) {
    Write-Host ""
    Write-Host "[WARN] 仍有残留 Python 进程:" -ForegroundColor Yellow
    foreach ($p in $pyProcs) {
        Write-Host "  PID=$($p.Id) Path=$($p.Path)"
    }
    Write-Host "  手动终止中..." -ForegroundColor Yellow
    $pyProcs | Stop-Process -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "=== 清理完成 ===" -ForegroundColor Cyan
Write-Host "保留的文件：spike 脚本、lock.json、README.md、protocol.md、decision.md"
if (-not $All) {
    Write-Host "results/ 已保留（加 -All 可一并删除）"
}
