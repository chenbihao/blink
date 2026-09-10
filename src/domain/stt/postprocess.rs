//! STT 文本后处理：尾部静音裁剪、幻觉语气词剥离、已确认前缀剥离。
//!
//! 这些函数由 `PseudoStreamingSttEngine` 在预览和定稿路径中调用，
//! 用于改善 SenseVoice 等多语言模型在中文语音识别中的已知问题。

/// 0.22.15：裁剪后保留的尾部缓冲（毫秒），避免切掉软辅音尾音。
const TRIM_TAIL_BUFFER_MS: u32 = 150;

/// 0.22.15：裁剪的最低阈值下界——即使 VAD off_threshold 很低，
/// 裁剪也不会低于此值，防止把极低振幅的环境噪声当成有声。
const TRIM_THRESHOLD_FLOOR: f64 = 0.0005;

/// 0.22.15：裁剪音频尾部的静音/低能量段——统一使用 VAD 的 off_threshold。
///
/// SenseVoice 等多语言模型在尾部静音上容易幻觉出英文语气词
///（如 "Yeah." "Okay."）。裁剪尾部静音可大幅减少此问题。
///
/// # 统一静音语义
///
/// 裁剪阈值取 `vad_off_threshold`（从 `EnergyVad::current_off_threshold()` 获取），
/// 不再用固定常量——确保"什么算静音"在 VAD 和裁剪之间一致。
/// `vad_off_threshold` 随 `noise_floor` 自适应变化。
///
/// # 全静音处理
///
/// 如果整段音频无任何样本超过阈值（全静音/no-speech），
/// 返回空 Vec——不把全静音送入 SenseVoice，避免诱发幻觉。
///
/// 算法：从末尾向前扫描，找到最后一个超过阈值的样本，
/// 保留该位置 + `TRIM_TAIL_BUFFER_MS` 缓冲后的部分。
pub(crate) fn trim_trailing_silence(
    samples: &[f32],
    sample_rate: u32,
    vad_off_threshold: f64,
) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    // 取 VAD off_threshold 与下界的较大值
    let threshold = (vad_off_threshold.max(TRIM_THRESHOLD_FLOOR)) as f32;

    // 从末尾向前找最后一个有声样本
    let mut last_audible = None;
    for (i, &s) in samples.iter().enumerate().rev() {
        if s.abs() > threshold {
            last_audible = Some(i);
            break;
        }
    }

    match last_audible {
        None => {
            // 0.22.15：全静音 → 返回空 Vec，不送入 SenseVoice
            Vec::new()
        }
        Some(idx) => {
            let buffer_samples = (TRIM_TAIL_BUFFER_MS as u64 * sample_rate as u64 / 1000) as usize;
            let end = (idx + 1 + buffer_samples).min(samples.len());
            samples[..end].to_vec()
        }
    }
}

/// SenseVoice 常见英文语气词幻觉。
///
/// 这些词在中文语音识别中不应出现，是多语言模型在静音段上的已知幻觉。
const FILLER_WORDS: &[&str] = &[
    "Yeah", "yeah", "Okay", "okay", "OK", "ok", "Mm", "mm", "Hmm", "hmm", "Uh", "uh", "Oh", "oh",
    "Ah", "ah", "Um", "um", "No", "no", "Yes", "yes", "Well", "well", "So", "so", "Right", "right",
    "Like", "like", "But", "but", "And", "and",
];

/// 判断字符是否为中文。
fn is_chinese_char(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}')
}

/// 剥离 SenseVoice 幻觉产生的尾部英文语气词。
///
/// 当识别文本以中文为主时，模型可能在尾部静音段幻觉出
/// 英文填充词（如 "Yeah." "Okay."）。此函数做后处理清理。
///
/// emoji 和 CJK 间空格已由 Python server `_postprocess_text` 处理，
/// 此处不再重复。
///
/// 仅当文本包含中文字符时才执行剥离，避免误伤纯英文识别。
pub(crate) fn strip_filler_words(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }

    // 检查是否包含中文字符
    let has_chinese = trimmed.chars().any(is_chinese_char);
    if !has_chinese {
        return trimmed.to_string();
    }

    let mut result = trimmed.to_string();

    // 循环剥离尾部语气词（可能多个连续出现）
    loop {
        let stripped = strip_one_filler_suffix(&result);
        if stripped.len() == result.len() {
            break;
        }
        result = stripped;
    }

    // 清理尾部残留的空格和标点
    result.trim_end().to_string()
}

/// 尝试从文本末尾剥离一个英文语气词后缀。
/// 返回剥离后的文本；如果没有匹配则原样返回。
fn strip_one_filler_suffix(text: &str) -> String {
    for &filler in FILLER_WORDS {
        let filler_lower = filler.to_lowercase();

        // 模式 1: "...中文 Yeah." → 匹配 " Yeah." / " Yeah," 等
        // 前面是空格或中文标点
        for &suffix in &[".", ",", "!", "?", ""] {
            let pattern = format!(" {}{}", filler_lower, suffix);
            if let Some(prefix) = strip_suffix_case_insensitive(text, &pattern) {
                return prefix.to_string();
            }
        }

        // 模式 2: "...中文Yeah." → 无空格直接拼接（较少见但存在）
        // 仅当 filler 前面是中文字符或中文标点时才匹配
        for &suffix in &[".", ",", "!", "?"] {
            let pattern = format!("{}{}", filler_lower, suffix);
            if let Some(prefix) = strip_suffix_case_insensitive(text, &pattern)
                && let Some(pc) = prefix.chars().next_back()
            {
                // 非 ASCII 字符 = 中文（汉字或标点）
                if !pc.is_ascii() {
                    return prefix.to_string();
                }
            }
        }
    }

    text.to_string()
}

/// 在原始字符串的 Unicode 字符边界上做不区分大小写的后缀匹配。
///
/// Unicode 小写映射可能改变 UTF-8 字节长度（例如 `İ` → `i` + 组合点），
/// 因此绝不能用 lowercased 字符串的字节长度反切原文。
fn strip_suffix_case_insensitive<'a>(text: &'a str, lowercase_suffix: &str) -> Option<&'a str> {
    text.char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .find_map(|index| {
            let suffix = text.get(index..)?;
            (suffix.to_lowercase() == lowercase_suffix).then(|| text.get(..index))?
        })
}

/// 从预览文本中剥离已确认的前缀部分。
///
/// 预览识别只取未确认音频，但模型仍可能因句子边界切分不完全
/// 而在 preview 开头重复部分 confirmed 文本。此函数做兜底清理：
///
/// 1. 精确前缀匹配 → 直接剥离
/// 2. 逐字符匹配 → 剥离匹配部分（应对标点差异等）
/// 3. 无匹配 → 原样返回
///
/// # 算法
///
/// 逐字符从开头比较 confirmed 和 preview，遇到第一个不匹配的字符停止。
/// 匹配长度 ≥ confirmed 长度的 50% 时才剥离（避免误剥离短公共前缀如"我"）。
pub(crate) fn strip_confirmed_prefix(confirmed: &str, preview: &str) -> String {
    if confirmed.is_empty() || preview.is_empty() {
        return preview.to_string();
    }

    // 1. 精确前缀匹配
    if let Some(stripped) = preview.strip_prefix(confirmed) {
        return stripped.to_string();
    }

    // 2. 逐字符匹配（应对标点差异）
    let confirmed_chars: Vec<char> = confirmed.chars().collect();
    let preview_chars: Vec<char> = preview.chars().collect();

    let mut match_len = 0;
    for (c, p) in confirmed_chars.iter().zip(preview_chars.iter()) {
        if c == p {
            match_len += 1;
        } else {
            break;
        }
    }

    // 匹配长度需达到 confirmed 的 50% 才剥离
    // 避免短公共前缀（如 "我"）导致误剥离
    if match_len > 0 && match_len * 2 >= confirmed_chars.len() {
        preview_chars[match_len..].iter().collect()
    } else {
        preview.to_string()
    }
}
