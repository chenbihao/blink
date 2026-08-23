<#
.SYNOPSIS
WinRT OCR baseline 生成脚本（兼容 PowerShell 5.1）

.DESCRIPTION
通过 C# interop 调用 WinRT OCR API，对 golden corpus 执行 OCR。
输出 results/winrt_baseline.json。

用法：
    powershell.exe -ExecutionPolicy Bypass -File winrt_baseline.ps1

约束：
    - 不接入生产 wiring
    - 必须对同一 corpus 输出 results/winrt_baseline.json
    - 输出总体和分 subset CER、line/word/rect 数据
#>

param(
    [string]$CorpusDir = (Resolve-Path "$PSScriptRoot\..\..\..\testdata\ocr\ppocrv6").Path,
    [string]$ResultsDir = "$PSScriptRoot\results"
)

$ErrorActionPreference = "Stop"

New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

Write-Host "=== WinRT OCR Baseline ===" -ForegroundColor Cyan
Write-Host "Corpus: $CorpusDir"
Write-Host "Results: $ResultsDir"

# 检查 manifest
$manifestPath = Join-Path $CorpusDir "manifest.json"
if (-not (Test-Path $manifestPath)) {
    throw "manifest.json not found: $manifestPath"
}

$manifest = Get-Content $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json

# -- Find Python for CER calculation --
$pythonExe = $null
$venvPython = Join-Path $PSScriptRoot ".venv\Scripts\python.exe"
if (Test-Path $venvPython) {
    $pythonExe = $venvPython
}
if (-not $pythonExe) {
    $sysPython = Get-Command python -ErrorAction SilentlyContinue
    if ($sysPython) {
        $pythonExe = $sysPython.Source
    }
}

# -- C# interop helper for WinRT OCR --
# Compiles a C# class that calls WinRT OCR API directly.
# This avoids PS 5.1's limitations with WinRT static methods.

$csCode = @"
using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices.WindowsRuntime;
using System.Threading.Tasks;
using Windows.Foundation;
using Windows.Graphics.Imaging;
using Windows.Media.Ocr;
using Windows.Storage.Streams;

public static class WinRtOcrHelper
{
    public static string[] GetAvailableLanguages()
    {
        var langs = OcrEngine.AvailableRecognizerLanguages();
        var result = new string[langs.Count];
        for (int i = 0; i < langs.Count; i++)
        {
            result[i] = langs[i].LanguageTag;
        }
        return result;
    }

    public static async Task<string> RunOcrAndGetJson(
        string imagePath, string expectedText, string subset,
        string langTag, string pythonExe)
    {
        // Read image bytes
        var pngBytes = File.ReadAllBytes(imagePath);

        // Create stream
        var stream = new InMemoryRandomAccessStream();
        await stream.WriteAsync(pngBytes.AsBuffer());
        stream.Seek(0);

        // Decode
        var decoder = await BitmapDecoder.CreateAsync(stream);
        var bitmap = await decoder.GetSoftwareBitmapAsync(
            BitmapPixelFormat.Bgra8, BitmapAlphaMode.Premultiplied);

        // Create OCR engine
        OcrEngine engine;
        if (!string.IsNullOrEmpty(langTag))
        {
            engine = new OcrEngine(new Windows.Globalization.Language(langTag));
        }
        else
        {
            engine = new OcrEngine();
        }

        // OCR
        var result = await engine.RecognizeAsync(bitmap);
        var ocrText = result.Text ?? "";

        // Word rect stats
        int wordCount = 0, validRectCount = 0, emptyRectCount = 0, outOfBoundsCount = 0;
        uint imgW = decoder.PixelWidth;
        uint imgH = decoder.PixelHeight;

        foreach (var line in result.Lines)
        {
            foreach (var word in line.Words)
            {
                wordCount++;
                var rect = word.BoundingRect;
                if (rect.Width == 0 && rect.Height == 0)
                    emptyRectCount++;
                else if (rect.X + rect.Width > imgW + 5 || rect.Y + rect.Height > imgH + 5)
                    outOfBoundsCount++;
                else
                    validRectCount++;
            }
        }

        // Compute CER
        double cer = ComputeCER(ocrText.Trim(), expectedText.Trim());

        // Cleanup
        bitmap.Dispose();
        stream.Dispose();

        // Return JSON
        var escapedOcr = ocrText.Replace("\\", "\\\\").Replace("\"", "\\\"").Replace("\n", "\\n").Replace("\r", "\\r");
        var json = string.Format(
            "{{\"image\":\"{0}\",\"subset\":\"{1}\",\"ocr_text\":\"{2}\",\"cer\":{3},\"word_count\":{4},\"valid_rects\":{5},\"empty_rects\":{6},\"out_of_bounds\":{7}}}",
            Path.GetFileName(imagePath), subset, escapedOcr,
            cer.ToString(System.Globalization.CultureInfo.InvariantCulture),
            wordCount, validRectCount, emptyRectCount, outOfBoundsCount);

        return json;
    }

    static double ComputeCER(string hyp, string reference)
    {
        if (string.IsNullOrEmpty(reference))
            return string.IsNullOrEmpty(hyp) ? 0.0 : 1.0;
        var h = hyp.ToCharArray();
        var r = reference.ToCharArray();
        int m = h.Length, n = r.Length;
        if (m == 0) return 1.0;
        var prev = new int[n + 1];
        var curr = new int[n + 1];
        for (int j = 0; j <= n; j++) prev[j] = j;
        for (int i = 1; i <= m; i++)
        {
            curr[0] = i;
            for (int j = 1; j <= n; j++)
            {
                int cost = h[i - 1] == r[j - 1] ? 0 : 1;
                curr[j] = Math.Min(Math.Min(prev[j] + 1, curr[j - 1] + 1), prev[j - 1] + cost);
            }
            var tmp = prev; prev = curr; curr = tmp;
        }
        return (double)prev[n] / n;
    }
}
"@

# Compile C# code with WinRT references
# PS 5.1: uses .NET Framework which has built-in WinRT support
Write-Host "[INFO] Compiling C# WinRT interop..." -ForegroundColor Yellow

$refAssemblies = @(
    "System.Runtime.dll",
    "System.Runtime.InteropServices.WindowsRuntime.dll",
    "System.Threading.Tasks.dll",
    "System.IO.dll"
)

try {
    Add-Type -TypeDefinition $csCode -Language CSharp -ReferencedAssemblies $refAssemblies -ErrorAction Stop
    Write-Host "[OK] C# interop compiled" -ForegroundColor Green
} catch {
    Write-Host "[INFO] Full compile failed, trying minimal refs..." -ForegroundColor Yellow
    try {
        Add-Type -TypeDefinition $csCode -Language CSharp -ErrorAction Stop
        Write-Host "[OK] C# interop compiled (minimal)" -ForegroundColor Green
    } catch {
        Write-Host "[ERROR] C# compilation failed: $_" -ForegroundColor Red
        throw "C# interop compilation failed"
    }
}

# Get available languages
$langTags = [WinRtOcrHelper]::GetAvailableLanguages()
Write-Host "Available OCR languages: $($langTags.Count)"
foreach ($t in $langTags) {
    Write-Host "  $t"
}

# Select Chinese-preferred language
$langTag = $null
$priorities = @("zh-Hans-CN", "zh-Hans", "zh-Hant-TW", "zh-Hant", "zh")
foreach ($p in $priorities) {
    if ($langTags -contains $p) {
        $langTag = $p
        break
    }
}
if (-not $langTag -and $langTags.Count -gt 0) {
    $langTag = $langTags[0]
}
Write-Host "Using language: $langTag"

# Process each corpus image
$allResults = @()
$itemIndex = 0

foreach ($item in $manifest.items) {
    $imgPath = Join-Path $CorpusDir $item.image
    $expectedText = $item.expected_text
    $subset = $item.subset

    if (-not (Test-Path $imgPath)) {
        Write-Host "[WARN] Image not found: $imgPath"
        continue
    }

    $itemIndex++
    Write-Host ("  [{0}/{1}] [{2}] {3}..." -f $itemIndex, $manifest.items.Count, $subset, (Split-Path $imgPath -Leaf)) -NoNewline

    # Call C# interop to run OCR
    $jsonResult = [WinRtOcrHelper]::RunOcrAndGetJson($imgPath, $expectedText, $subset, $langTag, $pythonExe).Result

    $parsed = $jsonResult | ConvertFrom-Json
    $allResults += $parsed

    Write-Host (" CER={0:F3} words={1} valid_rects={2}" -f $parsed.cer, $parsed.word_count, $parsed.valid_rects)
}

# Aggregate results
$subsetCers = @{}
$subsetRectStats = @{}
$allCers = @()
$totalWords = 0
$validWords = 0

foreach ($r in $allResults) {
    $cer = [double]$r.cer
    $allCers += $cer
    $totalWords += [int]$r.word_count
    $validWords += [int]$r.valid_rects

    $subset = $r.subset
    if (-not $subsetCers.ContainsKey($subset)) {
        $subsetCers[$subset] = @()
        $subsetRectStats[$subset] = @{ total_words = 0; valid_rects = 0; empty_rects = 0; out_of_bounds = 0 }
    }
    $subsetCers[$subset] += $cer
    $subsetRectStats[$subset].total_words += [int]$r.word_count
    $subsetRectStats[$subset].valid_rects += [int]$r.valid_rects
    $subsetRectStats[$subset].empty_rects += [int]$r.empty_rects
    $subsetRectStats[$subset].out_of_bounds += [int]$r.out_of_bounds
}

# Compute subset CER averages
$subsetResults = @{}
foreach ($kv in $subsetCers.GetEnumerator()) {
    $avg = ($kv.Value | Measure-Object -Average).Average
    $rs = $subsetRectStats[$kv.Key]
    $rvr = if ($rs.total_words -gt 0) { [Math]::Round($rs.valid_rects / $rs.total_words, 4) } else { 0.0 }
    $subsetResults[$kv.Key] = @{
        cer_mean = [Math]::Round($avg, 4)
        count = $kv.Value.Count
        total_words = $rs.total_words
        valid_rects = $rs.valid_rects
        empty_rects = $rs.empty_rects
        out_of_bounds = $rs.out_of_bounds
        rect_valid_ratio = $rvr
    }
}

$weightedCer = if ($allCers.Count -gt 0) { [Math]::Round(($allCers | Measure-Object -Average).Average, 4) } else { 1.0 }
$wordRectValidRatio = if ($totalWords -gt 0) { [Math]::Round($validWords / $totalWords, 4) } else { 0.0 }

$output = @{
    engine = "winrt-ocr"
    language = $langTag
    weighted_cer = $weightedCer
    total_items = $allCers.Count
    total_words = $totalWords
    valid_words = $validWords
    word_rect_valid_ratio = $wordRectValidRatio
    subsets = $subsetResults
}

$outputFile = "$ResultsDir\winrt_baseline.json"
$output | ConvertTo-Json -Depth 5 | Out-File -FilePath $outputFile -Encoding UTF8

Write-Host ""
Write-Host "=== WinRT Baseline Results ===" -ForegroundColor Cyan
Write-Host "Engine: $($output.engine)"
Write-Host "Language: $($output.language)"
Write-Host "Weighted CER: $($output.weighted_cer)"
Write-Host "Word rect valid ratio: $($output.word_rect_valid_ratio)"
Write-Host "Total items: $($output.total_items)"
Write-Host "Total words: $($output.total_words)"
Write-Host "Result file: $outputFile"
