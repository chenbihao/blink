[CmdletBinding()]
param(
    [int]$Port = 18080
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..\..'))
$cacheRoot = Join-Path $PSScriptRoot '.cache'
$downloadRoot = Join-Path $cacheRoot 'downloads'
$runtimeRoot = Join-Path $cacheRoot 'runtime'
$resultRoot = Join-Path $PSScriptRoot 'results'
$prefixRoot = Join-Path $cacheRoot 'prefixes'
$tempRoot = Join-Path $cacheRoot 'wrapper-temp'
$fixture = Join-Path $repoRoot 'testdata\stt\funasr-runtime\generated\blink-spike.wav'
$python = Join-Path $env:APPDATA 'blink\python\pythons\cpython-3.12.8-windows-x86_64-none\python.exe'
$wrapper = Join-Path $downloadRoot 'funasr_gguf_server-main.py'
$binary = Join-Path $runtimeRoot 'llama-funasr-sensevoice.exe'
$model = Join-Path $downloadRoot 'sensevoice-small-q8.gguf'
$vad = Join-Path $downloadRoot 'fsmn-vad.gguf'

foreach ($path in @($python, $wrapper, $binary, $model, $vad)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Missing spike prerequisite: $path"
    }
}

if (-not (Test-Path -LiteralPath $fixture -PathType Leaf) -or (Get-Item -LiteralPath $fixture).Length -eq 0) {
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'generate_fixture.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw 'Failed to generate the fixed WAV fixture'
    }
}

New-Item -ItemType Directory -Force -Path $resultRoot, $prefixRoot, $tempRoot | Out-Null
$prefixJson = & $python -I -S (Join-Path $PSScriptRoot 'make_prefixes.py') $fixture $prefixRoot
if ($LASTEXITCODE -ne 0) {
    throw 'Failed to generate WAV prefixes'
}
$prefixes = $prefixJson | ConvertFrom-Json

function Invoke-Transcription {
    param([Parameter(Mandatory)][string]$AudioPath)

    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $response = & curl.exe --fail --silent --show-error `
        -H 'X-Engine-Token: quick-spike' `
        -F 'model=funasr-gguf' `
        -F "file=@$AudioPath;type=audio/wav" `
        "http://127.0.0.1:$Port/v1/audio/transcriptions"
    $exitCode = $LASTEXITCODE
    $stopwatch.Stop()
    if ($exitCode -ne 0) {
        throw "Transcription failed for $AudioPath (curl exit $exitCode)"
    }
    $payload = ($response -join "`n") | ConvertFrom-Json
    [pscustomobject]@{
        audio = $AudioPath
        latency_ms = [Math]::Round($stopwatch.Elapsed.TotalMilliseconds, 1)
        text = [string]$payload.text
        non_empty = -not [string]::IsNullOrWhiteSpace([string]$payload.text)
    }
}

$process = $null
try {
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $python
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    @(
        '-I', '-S', $wrapper,
        '--host', '127.0.0.1', '--port', [string]$Port,
        '--binary', $binary, '--model', $model, '--vad', $vad,
        '--backend', 'cpu', '--work-dir', $tempRoot, '--timeout', '30'
    ) | ForEach-Object { [void]$startInfo.ArgumentList.Add($_) }
    $process = [Diagnostics.Process]::Start($startInfo)

    $readyWatch = [Diagnostics.Stopwatch]::StartNew()
    $health = $null
    for ($attempt = 0; $attempt -lt 50; $attempt++) {
        try {
            $health = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/health" -TimeoutSec 1
            break
        }
        catch {
            Start-Sleep -Milliseconds 100
        }
    }
    $readyWatch.Stop()
    if ($null -eq $health) {
        throw 'GGUF wrapper did not become ready'
    }

    $runs = foreach ($prefix in $prefixes) {
        $run = Invoke-Transcription -AudioPath ([string]$prefix.path)
        [pscustomobject]@{
            requested_seconds = $prefix.requested_seconds
            actual_seconds = [Math]::Round([double]$prefix.actual_seconds, 3)
            latency_ms = $run.latency_ms
            text = $run.text
            non_empty = $run.non_empty
        }
    }

    $result = [ordered]@{
        generated_at = [DateTimeOffset]::Now.ToString('o')
        wrapper_source = 'https://github.com/modelscope/FunASR/blob/main/runtime/llama.cpp/server/funasr_gguf_server.py'
        wrapper_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $wrapper).Hash.ToLowerInvariant()
        runtime = 'runtime-llamacpp-v0.2.6/windows-x64-portable'
        model = 'FunAudioLLM/SenseVoiceSmall-GGUF@sensevoice-small-q8.gguf'
        vad = 'FunAudioLLM/fsmn-vad-GGUF@fsmn-vad.gguf'
        python = (& $python -I -S -c 'import sys; print(sys.version.split()[0])')
        wrapper_ready_ms = [Math]::Round($readyWatch.Elapsed.TotalMilliseconds, 1)
        health = $health
        pseudo_streaming_runs = @($runs)
        residual_temp_files = @(
            Get-ChildItem -LiteralPath $tempRoot -File -ErrorAction SilentlyContinue |
                Select-Object -ExpandProperty FullName
        )
    }
    $resultPath = Join-Path $resultRoot 'quick-spike.json'
    $result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $resultPath -Encoding UTF8
    $result | ConvertTo-Json -Depth 8
}
finally {
    if ($null -ne $process -and -not $process.HasExited) {
        $process.Kill($true)
        [void]$process.WaitForExit(5000)
    }
}
