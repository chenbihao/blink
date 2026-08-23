# PowerShell AST Parser 检查脚本（使用 PowerShell 7 / pwsh）
$scripts = @(
    "xtask/spikes/ppocrv6/install.ps1",
    "xtask/spikes/ppocrv6/run_benchmark.ps1",
    "xtask/spikes/ppocrv6/evaluate.ps1",
    "xtask/spikes/ppocrv6/cache_tests.ps1",
    "xtask/spikes/ppocrv6/winrt_baseline.ps1"
)

$hasErrors = $false

Write-Host "=== PowerShell AST Parser checks (pwsh $($PSVersionTable.PSVersion)) ==="

foreach ($script in $scripts) {
    $fullPath = (Resolve-Path $script -ErrorAction SilentlyContinue).Path
    if (-not $fullPath) {
        Write-Host "  ${script}: NOT FOUND" -ForegroundColor Red
        $hasErrors = $true
        continue
    }

    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($fullPath, [ref]$tokens, [ref]$errors)

    if ($errors -and $errors.Count -gt 0) {
        Write-Host "  ${script}: FAIL ($($errors.Count) errors)" -ForegroundColor Red
        foreach ($e in $errors) {
            Write-Host "    Line $($e.Extent.StartLineNumber): $($e.Message)" -ForegroundColor Yellow
        }
        $hasErrors = $true
    } else {
        Write-Host "  ${script}: OK" -ForegroundColor Green
    }
}

if ($hasErrors) {
    Write-Host "`n[FAIL] Some scripts have syntax errors" -ForegroundColor Red
    exit 1
} else {
    Write-Host "`n[OK] All PowerShell scripts pass AST Parser checks" -ForegroundColor Green
    exit 0
}
