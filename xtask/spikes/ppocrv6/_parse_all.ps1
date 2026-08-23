$scripts = @(
    "xtask/spikes/ppocrv6/install.ps1",
    "xtask/spikes/ppocrv6/run_benchmark.ps1",
    "xtask/spikes/ppocrv6/evaluate.ps1",
    "xtask/spikes/ppocrv6/cache_tests.ps1",
    "xtask/spikes/ppocrv6/winrt_baseline.ps1"
)

Write-Host "PowerShell version: $($PSVersionTable.PSVersion.ToString())"
Write-Host ""

foreach ($script in $scripts) {
    $fullPath = (Resolve-Path $script -ErrorAction SilentlyContinue).Path
    if (-not $fullPath) {
        Write-Host "${script}: NOT FOUND"
        continue
    }

    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($fullPath, [ref]$tokens, [ref]$errors)

    if ($errors -and $errors.Count -gt 0) {
        Write-Host "${script}: FAIL ($($errors.Count) errors)"
        foreach ($e in $errors) {
            Write-Host "  L$($e.Extent.StartLineNumber) C$($e.Extent.StartColumnNumber): $($e.Message)"
        }
    } else {
        Write-Host "${script}: OK"
    }
}
