$manifest = Get-Content 'd:\Projects\Coding\blink\corpus\manifest.json' -Raw | ConvertFrom-Json
$selected = @('B2_275','B2_335','B2_313')
$manifest.samples = $manifest.samples | Where-Object { $selected -contains $_.sample_id }
$manifest | ConvertTo-Json -Depth 10 | Set-Content 'd:\Projects\Coding\blink\corpus\manifest_3sample.json' -Encoding UTF8
Write-Host "Done. Sample count: $($manifest.samples.Count)"
