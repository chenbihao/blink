$tokens = $null
$errors = $null
$path = (Resolve-Path "xtask/spikes/ppocrv6/install.ps1").Path
$ast = [System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
if ($errors -and $errors.Count -gt 0) {
    Write-Host "ERRORS: $($errors.Count)"
    foreach ($e in $errors) {
        Write-Host "  L$($e.Extent.StartLineNumber): $($e.Message)"
    }
} else {
    Write-Host "NO ERRORS"
}
