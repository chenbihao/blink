<#
.SYNOPSIS
PP-OCRv6 spike benchmark 运行脚本

.DESCRIPTION
默认只运行 thin/tiny 单次安全探测。全量三拓扑 × 三档模型 × 10 次必须显式传入
-FullSuite -AcknowledgeHardwareRisk，避免误触发持续满载。
- service/model 冷启动
- 首次识别 / 热识别延迟
- CPU 占用（实际采样）
- 峰值/稳定工作集
- venv + 模型磁盘占用
- 停止后进程回收

改进：
- 动态端口分配（避免 topology/model 切换时残留）
- stdout/stderr 持续异步排空（避免管道死锁）
- attempted/succeeded/failed 统计
- P95 使用线性插值算法（不退化）
- CPU 指标实际采样
- 电源模式实际记录
- worker topology 记录进程峰值

.NOTES
所有结果写入 results/ 目录（git-ignored）。
#>

param(
    [string]$VenvDir = "$PSScriptRoot\.venv",
    [string]$ModelCacheDir = "$PSScriptRoot\model-cache",
    [string]$ResultsDir = "$PSScriptRoot\results",
    [string]$CorpusDir = (Resolve-Path "$PSScriptRoot\..\..\..\testdata\ocr\ppocrv6").Path,
    [ValidateRange(1, 100)]
    [int]$Runs = 1,
    [ValidateSet("thin", "paddlex", "worker")]
    [string[]]$Topologies = @("thin"),
    [ValidateSet("tiny", "small", "medium")]
    [string[]]$Models = @("tiny"),
    [ValidateRange(1, 20)]
    [int]$HotRuns = 1,
    [ValidateRange(1, 16)]
    [int]$CpuThreads = 2,
    [ValidateRange(0, 300)]
    [int]$CooldownSeconds = 15,
    [switch]$EnableMkldnn,
    [switch]$FullSuite,
    [switch]$AcknowledgeHardwareRisk
)

$ErrorActionPreference = "Stop"
$venvPython = Join-Path $VenvDir "Scripts\python.exe"

if ($FullSuite) {
    $Runs = 10
    $Topologies = @("thin", "paddlex", "worker")
    $Models = @("tiny", "small", "medium")
    $HotRuns = 10
}

$estimatedRecognitions = 0
foreach ($topology in $Topologies) {
    $perRun = if ($topology -eq "worker") { 1 } else { 1 + $HotRuns }
    $estimatedRecognitions += $Models.Count * $Runs * $perRun
}
$isHighLoad = $FullSuite `
    -or $estimatedRecognitions -gt 6 `
    -or $CpuThreads -gt 4 `
    -or $EnableMkldnn `
    -or $Topologies -contains "paddlex" `
    -or $Models -contains "medium"
if ($isHighLoad -and -not $AcknowledgeHardwareRisk) {
    throw "拒绝启动高负载 benchmark（预计 $estimatedRecognitions 次识别，CPU threads=$CpuThreads，MKLDNN=$EnableMkldnn）。本机曾在 PaddleX 负载下出现 WHEA 0x124；确认硬件稳定后显式传入 -AcknowledgeHardwareRisk。"
}

Write-Host "安全配置: 预计识别 $estimatedRecognitions 次; CPU threads=$CpuThreads; MKLDNN=$EnableMkldnn; cooldown=${CooldownSeconds}s" -ForegroundColor Yellow

$threadEnvValue = $CpuThreads.ToString([System.Globalization.CultureInfo]::InvariantCulture)
foreach ($threadEnvName in @("OMP_NUM_THREADS", "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS", "NUMEXPR_NUM_THREADS", "PADDLE_PDX_CPU_NUM_THREADS")) {
    Set-Item -Path "Env:$threadEnvName" -Value $threadEnvValue
}
$env:BLINK_OCR_CPU_THREADS = $threadEnvValue
$env:BLINK_OCR_ENABLE_MKLDNN = if ($EnableMkldnn) { "1" } else { "0" }

if (-not (Test-Path $venvPython)) {
    throw "venv 不存在，请先运行 install.ps1"
}

New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

if (-not ("Blink.Benchmark.BoundedTextPump" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.IO;
using System.Text;
using System.Threading.Tasks;

namespace Blink.Benchmark {
    public sealed class BoundedTextPump {
        private readonly int _maxChars;
        private readonly StringBuilder _buffer = new StringBuilder();
        private readonly object _gate = new object();

        public BoundedTextPump(int maxChars) {
            _maxChars = maxChars;
        }

        public Task Start(StreamReader reader) {
            return Task.Run(async () => {
                string line;
                while ((line = await reader.ReadLineAsync().ConfigureAwait(false)) != null) {
                    lock (_gate) {
                        _buffer.AppendLine(line);
                        if (_buffer.Length > _maxChars) {
                            _buffer.Remove(0, _buffer.Length - _maxChars);
                        }
                    }
                }
            });
        }

        public string GetText() {
            lock (_gate) {
                return _buffer.ToString();
            }
        }
    }
}
"@
}

# ── 动态端口分配 ──
function Find-FreePort {
    param([int]$StartPort = 9200, [int]$EndPort = 9999)
    for ($p = $StartPort; $p -le $EndPort; $p++) {
        $inUse = $false
        try {
            $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, $p)
            $listener.Start()
            $listener.Stop()
        } catch {
            $inUse = $true
        }
        if (-not $inUse) { return $p }
    }
    throw "没有可用端口"
}

# ── 异步排空进程输出 ──
function Start-OutputPump {
    param($Process)
    $maxChars = 256KB
    $stdoutPump = [Blink.Benchmark.BoundedTextPump]::new($maxChars)
    $stderrPump = [Blink.Benchmark.BoundedTextPump]::new($maxChars)
    $stdoutJob = $stdoutPump.Start($Process.StandardOutput)
    $stderrJob = $stderrPump.Start($Process.StandardError)

    return @{
        StdoutPump = $stdoutPump
        StderrPump = $stderrPump
        StdoutTask = $stdoutJob
        StderrTask = $stderrJob
    }
}

function Set-BenchmarkEnvironment {
    param([System.Diagnostics.ProcessStartInfo]$StartInfo)

    $threadValue = $CpuThreads.ToString([System.Globalization.CultureInfo]::InvariantCulture)
    foreach ($name in @("OMP_NUM_THREADS", "MKL_NUM_THREADS", "OPENBLAS_NUM_THREADS", "NUMEXPR_NUM_THREADS", "PADDLE_PDX_CPU_NUM_THREADS")) {
        $StartInfo.Environment[$name] = $threadValue
    }
    $StartInfo.Environment["BLINK_OCR_CPU_THREADS"] = $threadValue
    $StartInfo.Environment["BLINK_OCR_ENABLE_MKLDNN"] = if ($EnableMkldnn) { "1" } else { "0" }
}

function Stop-BenchmarkProcess {
    param(
        [System.Diagnostics.Process]$Process,
        [int]$Port = 0,
        [string]$Token = ""
    )

    if ($null -eq $Process -or $Process.HasExited) { return $true }

    if ($Port -gt 0 -and $Token) {
        try {
            Invoke-RestMethod -Uri "http://127.0.0.1:$Port/shutdown" -Method Post `
                -Headers @{ "X-Engine-Token" = $Token } -TimeoutSec 3 | Out-Null
            $Process.WaitForExit(5000) | Out-Null
        } catch {}
    }

    if (-not $Process.HasExited) {
        try {
            # .NET 5+：递归终止整个派生进程树，避免 PaddleX child 泄漏。
            $Process.Kill($true)
        } catch {
            try { $Process.Kill() } catch {}
        }
        $Process.WaitForExit(5000) | Out-Null
    }

    return $Process.HasExited
}

function Get-ProcessTreeMetrics {
    param([int]$RootProcessId)

    $ids = [System.Collections.Generic.HashSet[int]]::new()
    [void]$ids.Add($RootProcessId)
    $treeComplete = $true
    try {
        $processRows = @(Get-CimInstance Win32_Process -ErrorAction Stop)
        $changed = $true
        while ($changed) {
            $changed = $false
            foreach ($row in $processRows) {
                if ($ids.Contains([int]$row.ParentProcessId) -and $ids.Add([int]$row.ProcessId)) {
                    $changed = $true
                }
            }
        }
    } catch {
        # 权限不足时至少保留根进程指标，并明确标记树不完整。
        $treeComplete = $false
    }

    [long]$workingSet = 0
    [long]$peakWorkingSet = 0
    [double]$cpuMs = 0
    $liveCount = 0
    foreach ($processId in $ids) {
        try {
            $process = Get-Process -Id $processId -ErrorAction Stop
            $workingSet += $process.WorkingSet64
            $peakWorkingSet += $process.PeakWorkingSet64
            $cpuMs += $process.TotalProcessorTime.TotalMilliseconds
            $liveCount++
        } catch {}
    }

    return @{
        working_set_bytes = $workingSet
        peak_working_set_bytes = $peakWorkingSet
        cpu_ms = $cpuMs
        process_count = $liveCount
        tree_complete = $treeComplete
    }
}

# ── P95 计算（线性插值法）──
function Get-PercentileLinear {
    param([double[]]$arr, [double]$p)
    if (-not $arr -or $arr.Count -eq 0) { return $null }
    $sorted = $arr | Sort-Object
    if ($sorted.Count -eq 1) { return [math]::Round($sorted[0], 2) }
    $rank = ($p / 100) * ($sorted.Count - 1)
    $lower = [math]::Floor($rank)
    $upper = [math]::Ceiling($rank)
    $fraction = $rank - $lower
    $value = $sorted[$lower] + $fraction * ($sorted[$upper] - $sorted[$lower])
    return [math]::Round($value, 2)
}

function Get-Percentile {
    param([double[]]$arr, [double]$p)
    return Get-PercentileLinear -arr $arr -p $p
}

# ── 选取测试图片 ──
$testImage = Join-Path $CorpusDir "medium" "medium-1.png"
if (-not (Test-Path $testImage)) {
    $testImage = Get-ChildItem -Path $CorpusDir -Filter "*.png" -Recurse | Select-Object -First 1
    if (-not $testImage) {
        throw "corpus 中没有 PNG 图片"
    }
    $testImage = $testImage.FullName
}
Write-Host "测试图片: $testImage" -ForegroundColor Cyan

# 读取图片为 base64
$imageBytes = [System.IO.File]::ReadAllBytes($testImage)
$imageB64 = [Convert]::ToBase64String($imageBytes)

# ── Benchmark 函数 ──

function Invoke-BenchmarkRun {
    param(
        [string]$Topology,
        [string]$Model,
        [int]$RunIdx
    )

    $token = [guid]::NewGuid().ToString("N")
    $port = Find-FreePort
    $result = @{
        topology = $Topology
        model = $Model
        run = $RunIdx
        port = $port
        attempted = $true
        succeeded = $false
    }

    $serverScript = $null
    switch ($Topology) {
        "thin" { $serverScript = "server_thin.py" }
        "paddlex" { $serverScript = "server_paddlex.py" }
        "worker" { $serverScript = $null }
    }

    if ($Topology -eq "worker") {
        # ── 单次 worker 拓扑 ──
        $t0 = Get-Date
        $workerProc = $null
        try {
            $workerPsi = [System.Diagnostics.ProcessStartInfo]::new()
            $workerPsi.FileName = $venvPython
            foreach ($argValue in @(
                "$PSScriptRoot\worker_once.py",
                "--image", $testImage,
                "--model", $Model,
                "--model-cache", $ModelCacheDir,
                "--cpu-threads", $CpuThreads.ToString()
            )) {
                [void]$workerPsi.ArgumentList.Add($argValue)
            }
            if ($EnableMkldnn) { [void]$workerPsi.ArgumentList.Add("--enable-mkldnn") }
            $workerPsi.UseShellExecute = $false
            $workerPsi.RedirectStandardOutput = $true
            $workerPsi.RedirectStandardError = $true
            $workerPsi.CreateNoWindow = $true
            Set-BenchmarkEnvironment -StartInfo $workerPsi

            $workerProc = [System.Diagnostics.Process]::Start($workerPsi)
            try { $workerProc.PriorityClass = [System.Diagnostics.ProcessPriorityClass]::BelowNormal } catch {}
            $workerPump = Start-OutputPump -Process $workerProc

            if (-not $workerProc.WaitForExit(180000)) {
                $result.error = "worker_timeout_after_180s"
                Stop-BenchmarkProcess -Process $workerProc | Out-Null
                return $result
            }
            [System.Threading.Tasks.Task]::WaitAll(@($workerPump.StdoutTask, $workerPump.StderrTask), 5000) | Out-Null
            $output = @($workerPump.StdoutPump.GetText(), $workerPump.StderrPump.GetText()) -split "`r?`n"
        } finally {
            if ($null -ne $workerProc -and -not $workerProc.HasExited) {
                Stop-BenchmarkProcess -Process $workerProc | Out-Null
            }
        }

        $t1 = Get-Date
        $coldStartMs = [math]::Round(($t1 - $t0).TotalMilliseconds, 2)

        $jsonLine = $output | Where-Object { $_ -match '^\{' } | Select-Object -Last 1
        if ($jsonLine) {
            try {
                $parsed = $jsonLine | ConvertFrom-Json
                if ($parsed.error) {
                    $result.error = $parsed.error
                    return $result
                }
                $result.cold_start_ms = $coldStartMs
                $result.load_ms = $parsed.load_ms
                $result.ocr_ms = $parsed.ocr_ms
                $result.first_recognize_ms = [math]::Round($coldStartMs, 2)
                $result.lines = $parsed.lines.Count
                $result.words = $parsed.words.Count
                $result.native_word_boxes = $parsed.native_word_boxes
                $result.fallback_word_boxes = $parsed.fallback_word_boxes
                $result.peak_working_set_mb = $parsed.peak_working_set_mb
                $result.succeeded = $true
            } catch {
                $result.error = "parse_failed: $_"
            }
        } else {
            $result.error = "no_json_output"
        }

        $result.hot_recognize_ms = $null

    } else {
        # ── 常驻服务拓扑 ──
        $serverPath = "$PSScriptRoot\$serverScript"
        $proc = $null

        try {

        # 启动服务
        $t_start = Get-Date
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $venvPython
        $mkldnnArg = if ($EnableMkldnn) { " --enable-mkldnn" } else { "" }
        $psi.Arguments = "`"$serverPath`" --port $port --model $Model --token $token --model-cache `"$ModelCacheDir`" --cpu-threads $CpuThreads$mkldnnArg"
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.CreateNoWindow = $true
        Set-BenchmarkEnvironment -StartInfo $psi

        $proc = [System.Diagnostics.Process]::Start($psi)
        try { $proc.PriorityClass = [System.Diagnostics.ProcessPriorityClass]::BelowNormal } catch {}

        # 异步排空 stdout/stderr
        $pump = Start-OutputPump -Process $proc

        # 等待服务 ready（轮询 /health）
        $healthReady = $false
        $modelReady = $false
        $maxWait = 120  # 秒（PaddleOCR 3.7 首次加载可能较慢）
        $waited = 0

        while (-not $healthReady -and $waited -lt $maxWait) {
            Start-Sleep -Milliseconds 200
            $waited += 0.2
            try {
                $resp = Invoke-RestMethod -Uri "http://127.0.0.1:$port/health" -Headers @{ "X-Engine-Token" = $token } -TimeoutSec 2 -ErrorAction Stop
                $healthReady = $true
                if ($resp.model_state -eq "Ready") {
                    $modelReady = $true
                }
                if ($resp.model_state -eq "Failed") {
                    $result.error = "model_failed"
                    break
                }
            } catch {
                # 还没 ready
            }
        }

        $t_health = Get-Date
        $serviceColdMs = [math]::Round(($t_health - $t_start).TotalMilliseconds, 2)

        # 等待模型 ready
        if ($healthReady -and -not $modelReady -and -not $result.error) {
            while (-not $modelReady -and $waited -lt $maxWait) {
                Start-Sleep -Milliseconds 200
                $waited += 0.2
                try {
                    $resp = Invoke-RestMethod -Uri "http://127.0.0.1:$port/health" -Headers @{ "X-Engine-Token" = $token } -TimeoutSec 2 -ErrorAction Stop
                    if ($resp.model_state -eq "Ready") {
                        $modelReady = $true
                    }
                    if ($resp.model_state -eq "Failed") {
                        $result.error = "model_failed_after_health"
                        break
                    }
                } catch {}
            }
        }

        $t_model = Get-Date
        $modelColdMs = [math]::Round(($t_model - $t_start).TotalMilliseconds, 2)

        if (-not $modelReady) {
            if (-not $result.error) {
                $result.error = "model_not_ready_after_${maxWait}s"
            }
            # 保存服务日志
            $stdoutText = $pump.StdoutPump.GetText()
            $stderrText = $pump.StderrPump.GetText()
            $result.server_stdout = $stdoutText.Substring(0, [math]::Min(2000, $stdoutText.Length))
            $result.server_stderr = $stderrText.Substring(0, [math]::Min(2000, $stderrText.Length))
            Stop-BenchmarkProcess -Process $proc -Port $port -Token $token | Out-Null
            return $result
        }

        $result.service_cold_ms = $serviceColdMs
        $result.model_ready_ms = $modelColdMs

        # ── 首次识别 ──
        $body = @{ image = $imageB64; request_id = "bench-${RunIdx}"; timeout_ms = 120000 } | ConvertTo-Json
        $t0 = Get-Date
        try {
            $resp = Invoke-RestMethod -Uri "http://127.0.0.1:$port/recognize" `
                -Method Post -Body $body `
                -Headers @{ "X-Engine-Token" = $token; "Content-Type" = "application/json" } `
                -TimeoutSec 180
            $t1 = Get-Date
            $result.first_recognize_ms = [math]::Round(($t1 - $t0).TotalMilliseconds, 2)
            $result.lines = $resp.lines.Count
            $result.words = $resp.words.Count
            $result.native_word_boxes = $resp.native_word_boxes
            $result.fallback_word_boxes = $resp.fallback_word_boxes
        } catch {
            $result.error = "first_recognize_failed: $_"
            $stdoutText = $pump.StdoutPump.GetText()
            $stderrText = $pump.StderrPump.GetText()
            $result.server_stdout = $stdoutText.Substring(0, [math]::Min(2000, $stdoutText.Length))
            $result.server_stderr = $stderrText.Substring(0, [math]::Min(2000, $stderrText.Length))
            Stop-BenchmarkProcess -Process $proc -Port $port -Token $token | Out-Null
            return $result
        }

        # ── 热识别（次数由 -HotRuns 控制）──
        $hotTimes = @()
        for ($i = 0; $i -lt $HotRuns; $i++) {
            $t0 = Get-Date
            try {
                $resp = Invoke-RestMethod -Uri "http://127.0.0.1:$port/recognize" `
                    -Method Post -Body $body `
                    -Headers @{ "X-Engine-Token" = $token; "Content-Type" = "application/json" } `
                    -TimeoutSec 60
                $t1 = Get-Date
                $hotTimes += [math]::Round(($t1 - $t0).TotalMilliseconds, 2)
            } catch {
                $hotTimes += -1
            }
        }
        $result.hot_recognize_samples = $hotTimes
        $validHotTimes = $hotTimes | Where-Object { $_ -gt 0 }
        if ($validHotTimes.Count -gt 0) {
            $result.hot_recognize_ms = [math]::Round(($validHotTimes | Measure-Object -Average).Average, 2)
            $result.hot_recognize_p50 = Get-Percentile $validHotTimes 50
            $result.hot_recognize_p95 = Get-Percentile $validHotTimes 95
        }
        if (@($validHotTimes).Count -ne $HotRuns) {
            $result.error = "hot_recognize_failed_$(@($validHotTimes).Count)_of_$HotRuns"
        }

        # ── 内存/CPU 采样（递归进程树；CIM 不可用时明确降级为根进程）──
        try {
            $procObj = Get-Process -Id $proc.Id -ErrorAction Stop
            $result.root_peak_working_set_mb = [math]::Round($procObj.PeakWorkingSet64 / 1MB, 1)

            $treeStart = Get-ProcessTreeMetrics -RootProcessId $proc.Id
            Start-Sleep -Milliseconds 500
            $treeEnd = Get-ProcessTreeMetrics -RootProcessId $proc.Id
            $result.peak_working_set_mb = [math]::Round($treeEnd.peak_working_set_bytes / 1MB, 1)
            $result.working_set_mb = [math]::Round($treeEnd.working_set_bytes / 1MB, 1)
            $result.process_count = $treeEnd.process_count
            $result.process_tree_complete = $treeStart.tree_complete -and $treeEnd.tree_complete
            $cpuDeltaMs = [math]::Max(0, $treeEnd.cpu_ms - $treeStart.cpu_ms)
            $result.cpu_percent = [math]::Round(
                $cpuDeltaMs / 500 * 100 / [Environment]::ProcessorCount,
                1
            )
        } catch {
            $result.peak_working_set_mb = -1
            $result.working_set_mb = -1
            $result.cpu_percent = -1
        }

        # ── 关闭服务（服务请求 + 递归进程树兜底）──
        $t_stop = Get-Date
        $shutdownOk = Stop-BenchmarkProcess -Process $proc -Port $port -Token $token

        $t_after = Get-Date
        $result.shutdown_ms = [math]::Round(($t_after - $t_stop).TotalMilliseconds, 2)

        # 验证进程已退出
        Start-Sleep -Milliseconds 500
        $procCheck = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
        $result.process_reclaimed = $shutdownOk -and ($null -eq $procCheck)

        $result.succeeded = $true
        } finally {
            if ($null -ne $proc -and -not $proc.HasExited) {
                Stop-BenchmarkProcess -Process $proc -Port $port -Token $token | Out-Null
            }
        }
    }

    return $result
}

# ── 记录机器信息（含电源模式）──

$machineFile = "$ResultsDir\machine_info.json"
if (-not (Test-Path $machineFile)) {
    $cpu = $null
    $os = $null
    $totalMem = $null
    try { $cpu = Get-CimInstance Win32_Processor -ErrorAction Stop | Select-Object -First 1 } catch {}
    try { $os = Get-CimInstance Win32_OperatingSystem -ErrorAction Stop } catch {}
    try {
        $computer = Get-CimInstance Win32_ComputerSystem -ErrorAction Stop
        $totalMem = [math]::Round($computer.TotalPhysicalMemory / 1GB, 1)
    } catch {}

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
    Write-Host "机器信息写入 $machineFile (电源模式: $powerMode)"
}

# ── 主循环 ──

$allResults = @()
$abortReason = $null

:benchmarkLoop foreach ($model in $Models) {
    foreach ($topo in $Topologies) {
        Write-Host ""
        Write-Host "=== $topo / $model ===" -ForegroundColor Cyan

        for ($i = 0; $i -lt $Runs; $i++) {
            Write-Host "  Run $($i + 1)/$Runs ... " -NoNewline
            $r = Invoke-BenchmarkRun -Topology $topo -Model $model -RunIdx $i
            $allResults += $r

            if ($r.error) {
                Write-Host "FAIL: $($r.error)" -ForegroundColor Red
            } elseif ($r.succeeded) {
                if ($r.hot_recognize_ms) {
                    Write-Host "OK hot=$($r.hot_recognize_ms)ms cold=$($r.model_ready_ms)ms ws=$($r.working_set_mb)MB cpu=$($r.cpu_percent)%" -ForegroundColor Green
                } elseif ($r.ocr_ms) {
                    Write-Host "OK ocr=$($r.ocr_ms)ms load=$($r.load_ms)ms" -ForegroundColor Green
                }
            } else {
                Write-Host "UNKNOWN" -ForegroundColor Yellow
            }

            if ($r.error -or ($r.ContainsKey("process_reclaimed") -and -not $r.process_reclaimed)) {
                $abortReason = if ($r.error) { $r.error } else { "process_tree_not_reclaimed" }
                Write-Warning "触发 fail-fast：$abortReason。剩余组合不再执行。"
                break benchmarkLoop
            }

            if ($CooldownSeconds -gt 0) {
                Start-Sleep -Seconds $CooldownSeconds
            }
        }
    }
}

# ── 汇总结果 ──

$summaryFile = "$ResultsDir\benchmark_raw.json"
ConvertTo-Json -InputObject @($allResults) -Depth 5 | Out-File -FilePath $summaryFile -Encoding utf8
Write-Host ""
Write-Host "=== Benchmark 完成 ===" -ForegroundColor Cyan
Write-Host "原始结果: $summaryFile"

# ── 计算 P50/P95 和统计 ──

$stats = @()
foreach ($model in $Models) {
    foreach ($topo in $Topologies) {
        $allRuns = @($allResults | Where-Object { $_.model -eq $model -and $_.topology -eq $topo })
        $successRuns = @($allRuns | Where-Object { $_.succeeded -and -not $_.error })
        $failRuns = @($allRuns | Where-Object { $_.error })
        $attempted = @($allRuns).Count
        $succeeded = @($successRuns).Count
        $failed = @($failRuns).Count

        if ($succeeded -eq 0) {
            $stats += @{
                topology = $topo
                model = $model
                attempted = $attempted
                succeeded = $succeeded
                failed = $failed
                qualified = $false
                reason = "no_successful_runs"
            }
            continue
        }

        $hotMs = $successRuns | ForEach-Object { $_.hot_recognize_ms } | Where-Object { $_ -ne $null -and $_ -gt 0 } | Sort-Object
        $coldMs = $successRuns | ForEach-Object { $_.model_ready_ms } | Where-Object { $_ -ne $null -and $_ -gt 0 } | Sort-Object
        $firstMs = $successRuns | ForEach-Object { $_.first_recognize_ms } | Where-Object { $_ -ne $null -and $_ -gt 0 } | Sort-Object
        $peakWs = $successRuns | ForEach-Object { $_.peak_working_set_mb } | Where-Object { $_ -ne $null -and $_ -gt 0 } | Sort-Object
        $ws = $successRuns | ForEach-Object { $_.working_set_mb } | Where-Object { $_ -ne $null -and $_ -gt 0 } | Sort-Object
        $cpuPct = $successRuns | ForEach-Object { $_.cpu_percent } | Where-Object { $_ -ne $null -and $_ -gt 0 } | Sort-Object

        $stat = @{
            topology = $topo
            model = $model
            attempted = $attempted
            succeeded = $succeeded
            failed = $failed
            qualified = ($succeeded -ge 10)
            hot_p50 = Get-Percentile $hotMs 50
            hot_p95 = Get-Percentile $hotMs 95
            cold_p50 = Get-Percentile $coldMs 50
            cold_p95 = Get-Percentile $coldMs 95
            first_p50 = Get-Percentile $firstMs 50
            first_p95 = Get-Percentile $firstMs 95
            peak_ws_p95 = Get-Percentile $peakWs 95
            ws_p50 = Get-Percentile $ws 50
            cpu_avg = if ($cpuPct.Count -gt 0) { [math]::Round(($cpuPct | Measure-Object -Average).Average, 1) } else { $null }
        }
        $stats += $stat
    }
}

$statsFile = "$ResultsDir\benchmark_stats.json"
ConvertTo-Json -InputObject @($stats) -Depth 5 | Out-File -FilePath $statsFile -Encoding utf8
Write-Host "统计结果: $statsFile"

# 打印汇总表
Write-Host ""
Write-Host "=== P50/P95 汇总 ===" -ForegroundColor Cyan
Write-Host ("{0,-11} {1,-8} {2,3}/{3,3}/{4,3} {5,7}/{6,7}ms {7,6}/{8,6}ms {9,8}MB {10,8}MB {11,5}%" -f `
    "Topology", "Model", "att", "ok", "fail", "ColdP50", "ColdP95", "HotP50", "HotP95", "PeakWS", "WS", "CPU")
foreach ($s in $stats) {
    $line = "{0,-11} {1,-8} {2,3}/{3,3}/{4,3} {5,7}/{6,7}ms {7,6}/{8,6}ms {9,8}MB {10,8}MB {11,5}%" -f `
        $s.topology, $s.model, `
        $s.attempted, $s.succeeded, $s.failed, `
        $s.cold_p50, $s.cold_p95, `
        $s.hot_p50, $s.hot_p95, `
        $s.peak_ws_p95, $s.ws_p50, `
        $s.cpu_avg
    Write-Host $line
}

if ($abortReason) {
    throw "Benchmark 已安全中止：$abortReason"
}
