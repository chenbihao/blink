[CmdletBinding()]
param(
    [string]$OutputDirectory
)

# 生成 0.22.8 FSMN-VAD spike 所需的全部可再分发测试音频。
#
# 音频来源：Windows SAPI 语音合成 + 数学生成正弦波/白噪声。
# 不含任何私人录音。所有音频 16kHz mono PCM s16le WAV。
#
# 测试矩阵覆盖 10 种场景：
#   1. zh_short       — 中文短句（~2s）
#   2. zh_long        — 中文长句（~6s）
#   3. mid_pause     — 句中停顿（说话-停顿-继续说话）
#   4. think_pause    — 思考停顿（长停顿后继续，~5s 总）
#   5. low_volume     — 低音量语音
#   6. far_field      — 远场模拟（低音量 + 轻微噪声）
#   7. steady_noise   — 稳态背景噪声
#   8. burst_noise    — 突发噪声
#   9. pure_silence   — 纯静音
#  10. cough_burst    — 咳嗽/爆音

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$resolvedOutput = if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    [System.IO.Path]::GetFullPath(
        (Join-Path $PSScriptRoot 'fixtures\audio')
    )
} else {
    [System.IO.Path]::GetFullPath($OutputDirectory)
}
$allowedRoot = [System.IO.Path]::GetFullPath(
    (Join-Path $PSScriptRoot 'fixtures')
)
if (-not $resolvedOutput.StartsWith($allowedRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Fixture output must stay under $allowedRoot"
}

New-Item -ItemType Directory -Force -Path $resolvedOutput | Out-Null

# ── SAPI 语音合成 ──
$synth = [System.Speech.Synthesis.SpeechSynthesizer]::new()
try {
    $voice = $synth.GetInstalledVoices() |
        Where-Object { $_.Enabled } |
        Select-Object -First 1
    if ($null -eq $voice) {
        throw 'No enabled Windows SAPI voice is available'
    }
    $synth.SelectVoice($voice.VoiceInfo.Name)
    $synth.Rate = 0
    $synth.Volume = 100

    # 1. zh_short
    $synth.SetOutputToWaveFile((Join-Path $resolvedOutput 'zh_short.wav'))
    $synth.Speak('你好世界。')
    $synth.SetOutputToDefaultAudioDevice()

    # 2. zh_long
    $synth.SetOutputToWaveFile((Join-Path $resolvedOutput 'zh_long.wav'))
    $synth.Speak('今天天气真不错我想出去走走顺便买点东西回来做晚饭。')
    $synth.SetOutputToDefaultAudioDevice()

    # 3. mid_pause: SAPI 不支持 SSML pause in wave file output,
    #    so we concatenate two synth clips with a silence gap.
    $synth.SetOutputToWaveFile((Join-Path $resolvedOutput 'mid_pause_part1.wav'))
    $synth.Speak('我想去超市')
    $synth.SetOutputToWaveFile((Join-Path $resolvedOutput 'mid_pause_part2.wav'))
    $synth.Speak('买点水果。')
    $synth.SetOutputToDefaultAudioDevice()

    # 4. think_pause
    $synth.SetOutputToWaveFile((Join-Path $resolvedOutput 'think_pause_part1.wav'))
    $synth.Speak('让我想想')
    $synth.SetOutputToWaveFile((Join-Path $resolvedOutput 'think_pause_part2.wav'))
    $synth.Speak('对了，明天开会。')
    $synth.SetOutputToDefaultAudioDevice()

    # 5. low_volume: synth at reduced volume then post-process amplitude
    $synth.SetOutputToWaveFile((Join-Path $resolvedOutput 'low_volume_src.wav'))
    $synth.Speak('这是一个低音量的测试。')
    $synth.SetOutputToDefaultAudioDevice()
}
finally {
    $synth.Dispose()
}

# ── WAV 工具函数 ──
# 生成 16kHz mono s16le WAV 文件的原始字节
function New-WavBytes {
    param(
        [int]$SampleRate = 16000,
        [int16[]]$Samples
    )
    $dataBytes = [byte[]]::new($Samples.Length * 2)
    [Buffer]::BlockCopy($Samples, 0, $dataBytes, 0, $dataBytes.Length)
    $header = [byte[]]::new(44)
    # RIFF header
    [Encoding]::ASCII.GetBytes('RIFF').CopyTo($header, 0)
    $fileSize = 36 + $dataBytes.Length
    [BitConverter]::GetBytes([uint32]$fileSize).CopyTo($header, 4)
    [Encoding]::ASCII.GetBytes('WAVE').CopyTo($header, 8)
    # fmt chunk
    [Encoding]::ASCII.GetBytes('fmt ').CopyTo($header, 12)
    [BitConverter]::GetBytes([uint32]16).CopyTo($header, 16)
    [BitConverter]::GetBytes([uint16]1).CopyTo($header, 20)  # PCM
    [BitConverter]::GetBytes([uint16]1).CopyTo($header, 22)  # mono
    [BitConverter]::GetBytes([uint32]$SampleRate).CopyTo($header, 24)
    [BitConverter]::GetBytes([uint32]($SampleRate * 2)).CopyTo($header, 28)  # byte rate
    [BitConverter]::GetBytes([uint16]2).CopyTo($header, 32)  # block align
    [BitConverter]::GetBytes([uint16]16).CopyTo($header, 34)  # bits per sample
    # data chunk
    [Encoding]::ASCII.GetBytes('data').CopyTo($header, 36)
    [BitConverter]::GetBytes([uint32]$dataBytes.Length).CopyTo($header, 40)
    return $header + $dataBytes
}

# 生成正弦波采样
function New-ToneSamples {
    param([int]$SampleRate, [double]$DurationSec, [double]$Freq, [double]$Amplitude)
    $n = [int]($SampleRate * $DurationSec)
    $samples = [int16[]]::new($n)
    for ($i = 0; $i -lt $n; $i++) {
        $t = $i / $SampleRate
        $val = $Amplitude * [Math]::Sin(2.0 * [Math]::PI * $Freq * $t)
        $samples[$i] = [int16]($val * 32767)
    }
    return $samples
}

# 生成白噪声采样
function New-NoiseSamples {
    param([int]$SampleRate, [double]$DurationSec, [double]$Amplitude)
    $n = [int]($SampleRate * $DurationSec)
    $samples = [int16[]]::new($n)
    $rng = [System.Random]::new(42)  # 固定种子，可复现
    for ($i = 0; $i -lt $n; $i++) {
        $val = ($rng.NextDouble() * 2.0 - 1.0) * $Amplitude
        $samples[$i] = [int16]($val * 32767)
    }
    return $samples
}

# 生成静音采样
function New-SilenceSamples {
    param([int]$SampleRate, [double]$DurationSec)
    $n = [int]($SampleRate * $DurationSec)
    return [int16[]]::new($n)  # 全零
}

# 合并采样数组
function Merge-Samples {
    param([params()][int16[][]]$Arrays)
    $totalLen = 0
    foreach ($a in $Arrays) { $totalLen += $a.Length }
    $result = [int16[]]::new($totalLen)
    $offset = 0
    foreach ($a in $Arrays) {
        [Array]::Copy($a, 0, $result, $offset, $a.Length)
        $offset += $a.Length
    }
    return $result
}

# 3. mid_pause: speech(1.5s) + silence(500ms) + speech(1.5s)
$part1 = New-ToneSamples 16000 1.5 200 0.15  # 模拟语音
$silence = New-SilenceSamples 16000 0.5
$part2 = New-ToneSamples 16000 1.5 200 0.15
$midPause = Merge-Samples @($part1, $silence, $part2)
[System.IO.File]::WriteAllBytes(
    (Join-Path $resolvedOutput 'mid_pause.wav'),
    (New-WavBytes -Samples $midPause))

# 4. think_pause: speech(1s) + long silence(3s) + speech(1.5s)
$thinkP1 = New-ToneSamples 16000 1.0 200 0.15
$longSilence = New-SilenceSamples 16000 3.0
$thinkP2 = New-ToneSamples 16000 1.5 200 0.15
$thinkPause = Merge-Samples @($thinkP1, $longSilence, $thinkP2)
[System.IO.File]::WriteAllBytes(
    (Join-Path $resolvedOutput 'think_pause.wav'),
    (New-WavBytes -Samples $thinkPause))

# 5. low_volume: reduced amplitude tone
$lowVol = New-ToneSamples 16000 2.0 200 0.003  # 0.003 amplitude ≈ -50dB
[System.IO.File]::WriteAllBytes(
    (Join-Path $resolvedOutput 'low_volume.wav'),
    (New-WavBytes -Samples $lowVol))

# 6. far_field: low amplitude tone + low noise
$farTone = New-ToneSamples 16000 2.0 200 0.005
$farNoise = New-NoiseSamples 16000 2.0 0.002
$farSamples = [int16[]]::new($farTone.Length)
for ($i = 0; $i -lt $farTone.Length; $i++) {
    $combined = ($farTone[$i] + $farNoise[$i]) / 2
    $farSamples[$i] = [int16]([Math]::Max(-32768, [Math]::Min(32767, $combined)))
}
[System.IO.File]::WriteAllBytes(
    (Join-Path $resolvedOutput 'far_field.wav'),
    (New-WavBytes -Samples $farSamples))

# 7. steady_noise: continuous white noise
$steadyNoise = New-NoiseSamples 16000 3.0 0.01
[System.IO.File]::WriteAllBytes(
    (Join-Path $resolvedOutput 'steady_noise.wav'),
    (New-WavBytes -Samples $steadyNoise))

# 8. burst_noise: silence + short noise burst + silence
$bSilence1 = New-SilenceSamples 16000 1.0
$bBurst = New-NoiseSamples 16000 0.3 0.05
$bSilence2 = New-SilenceSamples 16000 1.0
$burstNoise = Merge-Samples @($bSilence1, $bBurst, $bSilence2)
[System.IO.File]::WriteAllBytes(
    (Join-Path $resolvedOutput 'burst_noise.wav'),
    (New-WavBytes -Samples $burstNoise))

# 9. pure_silence
$pureSilence = New-SilenceSamples 16000 2.0
[System.IO.File]::WriteAllBytes(
    (Join-Path $resolvedOutput 'pure_silence.wav'),
    (New-WavBytes -Samples $pureSilence))

# 10. cough_burst: silence + burst + silence + tone + silence
$cSilence1 = New-SilenceSamples 16000 0.5
$cBurst = New-NoiseSamples 16000 0.15 0.08
$cSilence2 = New-SilenceSamples 16000 0.2
$cTone = New-ToneSamples 16000 1.5 200 0.15
$cSilence3 = New-SilenceSamples 16000 0.5
$coughBurst = Merge-Samples @($cSilence1, $cBurst, $cSilence2, $cTone, $cSilence3)
[System.IO.File]::WriteAllBytes(
    (Join-Path $resolvedOutput 'cough_burst.wav'),
    (New-WavBytes -Samples $coughBurst))

# ── 清理临时 SAPI 分段文件 ──
$temps = @(
    'mid_pause_part1.wav', 'mid_pause_part2.wav',
    'think_pause_part1.wav', 'think_pause_part2.wav',
    'low_volume_src.wav'
)
foreach ($t in $temps) {
    $tp = Join-Path $resolvedOutput $t
    if (Test-Path $tp) { Remove-Item $tp -Force }
}

# ── 输出清单 ──
$manifest = @{
    generated_at = (Get-Date).ToString('o')
    sample_rate = 16000
    channels = 1
    format = 'PCM s16le WAV'
    source = 'Windows SAPI synthesis + mathematical generation'
    privacy = 'No private recordings. All audio is machine-generated.'
    files = @()
}

Get-ChildItem -Path $resolvedOutput -Filter '*.wav' | Sort-Object Name | ForEach-Object {
    $hash = Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName
    $manifest.files += @{
        name = $_.Name
        bytes = $_.Length
        sha256 = $hash.Hash.ToLowerInvariant()
    }
}

$manifest | ConvertTo-Json -Depth 5 | Set-Content -Path (Join-Path $resolvedOutput 'manifest.json') -Encoding UTF8
Write-Host "Generated $($manifest.files.Count) fixture files in $resolvedOutput"
$manifest | ConvertTo-Json -Depth 5
