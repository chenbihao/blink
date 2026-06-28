<#
.SYNOPSIS
Blink 插件打包脚本 - Tauri beforeBuildCommand
.DESCRIPTION
1. 编译所有 Rust 插件（Release 模式）
2. 把 exe 复制到 plugins/builtin/<id>/bin/ 目录（随 MSI 打包）
#>

$ErrorActionPreference = "Stop"

# CI 下插件已由 workflow 的独立 step 预编译完成，beforeBuildCommand 再次触发本脚本时
# 直接短路退出，避免重复编译。本地 cargo tauri build 不会设置此变量，照常编译。
if ($env:BLINK_SKIP_PLUGIN_BUILD -eq "1") {
    Write-Host "⏭️ BLINK_SKIP_PLUGIN_BUILD=1，跳过插件编译（已由 CI 预编译步骤完成）" -ForegroundColor DarkGray
    exit 0
}

$RootDir = Split-Path $PSScriptRoot -Parent
$TargetRelease = Join-Path $RootDir "target/release"
$BuiltinDir = Join-Path $RootDir "plugins/builtin"

Write-Host "📦 Phase 0.6: Blink 插件打包开始..." -ForegroundColor Cyan
Write-Host "  Root: $RootDir"
Write-Host "  Target: $TargetRelease"
Write-Host ""

# ── 1. 编译所有 Rust 插件 ──────────────────────────────────────
Write-Host "🔨 编译 Rust 插件（Release 模式）..." -ForegroundColor Yellow

$RustPlugins = @("echo", "ip", "weather")
foreach ($id in $RustPlugins) {
    Write-Host "  编译: blink-plugin-$id"
    $process = Start-Process -FilePath "cargo" -ArgumentList "build", "--release", "--bin", "blink-plugin-$id" -NoNewWindow -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        throw "插件 $id 编译失败，ExitCode: $($process.ExitCode)"
    }
}

Write-Host ""

# ── 2. 复制 exe 到 builtin 插件的 bin 目录 ────────────────────
Write-Host "📋 复制插件编译产物..." -ForegroundColor Yellow

foreach ($id in $RustPlugins) {
    $PluginBin = Join-Path $BuiltinDir "$id/bin"
    $SourceExe = Join-Path $TargetRelease "blink-plugin-$id.exe"
    $DestExe = Join-Path $PluginBin "blink-plugin-$id.exe"

    if (!(Test-Path $PluginBin)) {
        New-Item -ItemType Directory -Path $PluginBin -Force | Out-Null
    }

    Copy-Item $SourceExe $DestExe -Force
    Write-Host "  ✓ $id -> $DestExe"
}

Write-Host ""

# ── 3. 脚本插件不需要处理（源码已在 builtin 下）──────────────
Write-Host "🐍 脚本插件无需编译（Python/Node.js 源码已在 builtin 下）" -ForegroundColor Green

Write-Host ""
Write-Host "✅ 插件打包完成" -ForegroundColor Green
