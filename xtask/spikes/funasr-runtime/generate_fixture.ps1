[CmdletBinding()]
param(
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$resolvedOutput = if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    [System.IO.Path]::GetFullPath(
        (Join-Path $PSScriptRoot '..\..\..\testdata\stt\funasr-runtime\generated')
    )
}
else {
    [System.IO.Path]::GetFullPath($OutputDirectory)
}
$allowedRoot = [System.IO.Path]::GetFullPath(
    (Join-Path $PSScriptRoot '..\..\..\testdata\stt\funasr-runtime')
)
if (-not $resolvedOutput.StartsWith($allowedRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Fixture output must stay under $allowedRoot"
}

New-Item -ItemType Directory -Force -Path $resolvedOutput | Out-Null
Add-Type -AssemblyName System.Speech

$synth = [System.Speech.Synthesis.SpeechSynthesizer]::new()
try {
    $voice = $synth.GetInstalledVoices() |
        Where-Object { $_.Enabled } |
        Select-Object -First 1
    if ($null -eq $voice) {
        throw 'No enabled Windows SAPI voice is available'
    }

    $synth.SelectVoice($voice.VoiceInfo.Name)
    $outputPath = Join-Path $resolvedOutput 'blink-spike.wav'
    $synth.SetOutputToWaveFile($outputPath)
    $synth.Speak('Hello. This is a fixed speech recognition test for Blink.')
    $synth.SetOutputToDefaultAudioDevice()

    $file = Get-Item -LiteralPath $outputPath
    $hash = Get-FileHash -Algorithm SHA256 -LiteralPath $outputPath
    [pscustomobject]@{
        path = $file.FullName
        bytes = $file.Length
        sha256 = $hash.Hash.ToLowerInvariant()
        voice = $voice.VoiceInfo.Name
        culture = $voice.VoiceInfo.Culture.Name
        text = 'Hello. This is a fixed speech recognition test for Blink.'
    } | ConvertTo-Json
}
finally {
    $synth.Dispose()
}
