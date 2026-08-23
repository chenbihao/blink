$venvPython = "$PSScriptRoot\.venv\Scripts\python.exe"
$port = 9300
$token = "test123"

Write-Host "Starting server_thin.py..."

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $venvPython
$psi.Arguments = "`"$PSScriptRoot\server_thin.py`" --port $port --model tiny --token $token --model-cache `"$PSScriptRoot\model-cache`""
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.CreateNoWindow = $true

$proc = [System.Diagnostics.Process]::Start($psi)

Write-Host "PID: $($proc.Id)"

# Async read output
$stdoutTask = $proc.StandardOutput.ReadToEndAsync()
$stderrTask = $proc.StandardError.ReadToEndAsync()

# Wait for health
$maxWait = 120
$waited = 0
$modelReady = $false

while ($waited -lt $maxWait) {
    Start-Sleep -Seconds 2
    $waited += 2
    try {
        $h = Invoke-RestMethod -Uri "http://127.0.0.1:$port/health" -Headers @{"X-Engine-Token" = $token} -TimeoutSec 3
        Write-Host "Health: model_state=$($h.model_state)"
        if ($h.model_state -eq "Ready") {
            $modelReady = $true
            break
        }
        if ($h.model_state -eq "Failed") {
            Write-Host "Model FAILED!"
            break
        }
    } catch {
        Write-Host "  Waiting... ($waited s)"
    }
}

if ($modelReady) {
    Write-Host "Model ready! Sending small image..."

    # Use a small image first
    $imgPath = "$PSScriptRoot\..\..\..\testdata\ocr\ppocrv6\chinese\basic-1.png"
    $imgB64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($imgPath))
    $body = @{ image = $imgB64; request_id = "test1"; timeout_ms = 120000 } | ConvertTo-Json

    $t0 = Get-Date
    try {
        $resp = Invoke-RestMethod -Uri "http://127.0.0.1:$port/recognize" `
            -Method Post -Body $body `
            -Headers @{"X-Engine-Token" = $token; "Content-Type" = "application/json"} `
            -TimeoutSec 180
        $t1 = Get-Date
        Write-Host "OCR took: $([math]::Round(($t1 - $t0).TotalMilliseconds, 2))ms"
        Write-Host "Lines: $($resp.lines.Count)"
        Write-Host "Words: $($resp.words.Count)"
        Write-Host "Native word boxes: $($resp.native_word_boxes)"

        # Now test the large image
        Write-Host ""
        Write-Host "Testing large image (medium-1.png 2560x1440)..."
        $imgPath2 = "$PSScriptRoot\..\..\..\testdata\ocr\ppocrv6\medium\medium-1.png"
        $imgB642 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($imgPath2))
        $body2 = @{ image = $imgB642; request_id = "test2"; timeout_ms = 300000 } | ConvertTo-Json

        $t0 = Get-Date
        $resp2 = Invoke-RestMethod -Uri "http://127.0.0.1:$port/recognize" `
            -Method Post -Body $body2 `
            -Headers @{"X-Engine-Token" = $token; "Content-Type" = "application/json"} `
            -TimeoutSec 300
        $t1 = Get-Date
        Write-Host "Large OCR took: $([math]::Round(($t1 - $t0).TotalMilliseconds, 2))ms"
        Write-Host "Lines: $($resp2.lines.Count)"
        Write-Host "Words: $($resp2.words.Count)"
        Write-Host "Native word boxes: $($resp2.native_word_boxes)"
    } catch {
        Write-Host "OCR failed: $_"
    }
} else {
    Write-Host "Model not ready after $maxWait s"
}

# Shutdown
if (-not $proc.HasExited) {
    try { Invoke-RestMethod -Uri "http://127.0.0.1:$port/shutdown" -Method Post -Headers @{"X-Engine-Token" = $token} -TimeoutSec 5 } catch {}
    Start-Sleep 3
    if (-not $proc.HasExited) { $proc.Kill() }
}

# Show stderr
Write-Host ""
Write-Host "--- stderr (last 20 lines) ---"
$stderr = $stderrTask.Result
$stderrLines = $stderr -split "`n"
$stderrLines | Select-Object -Last 20 | ForEach-Object { Write-Host $_ }
