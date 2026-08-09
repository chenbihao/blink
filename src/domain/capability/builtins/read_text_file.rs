//! `read_text_file` Capability（0.19.5）。
//!
//! 以绝对路径为稳定引用，按 metadata/range/head/tail 四种模式有界读取 UTF-8 文本。
//! 读取器逐块寻找换行，单行超过上限时立即失败，不会把无换行的大文件整体载入内存。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::domain::ai::memory::estimate_tokens;
use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext, ItemResult,
};

const PROBE_BYTES: usize = 8 * 1024;
const MAX_LINE_BYTES: usize = 32 * 1024;
const MAX_CONTENT_BYTES: usize = 32 * 1024;
const SOFT_TOKEN_BUDGET: usize = 2_000;
const HARD_TOKEN_LIMIT: usize = 4_000;
const MAX_REQUEST_LINES: u64 = 1_000;

pub struct ReadTextFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    Metadata,
    Range,
    Head,
    Tail,
}

#[derive(Debug)]
struct ReadRequest {
    path: PathBuf,
    mode: ReadMode,
    line: Option<u64>,
    limit: Option<u64>,
}

#[derive(Debug)]
struct TextMetadata {
    size_bytes: u64,
    modified_at: Option<i64>,
    text_status: &'static str,
}

#[derive(Debug)]
struct TextPage {
    content: String,
    start_line: Option<u64>,
    end_line: Option<u64>,
    eof: bool,
    truncated_by_budget: bool,
    next_line: Option<u64>,
    bytes_read: u64,
    content_bytes: usize,
    estimated_tokens: usize,
}

#[derive(Debug)]
struct BufferedLine {
    number: u64,
    text: String,
    bytes: usize,
    tokens: usize,
}

#[async_trait::async_trait]
impl Capability for ReadTextFile {
    fn id(&self) -> &str {
        "read_text_file"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "受控读取本地 UTF-8 文本文件。仅接受绝对路径；支持 metadata/range/head/tail，正文按行分页并受 token、字节和行数上限约束。".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "description": "文件绝对路径" },
                    "mode": { "type": "string", "enum": ["metadata", "range", "head", "tail"] },
                    "line": { "type": "integer", "minimum": 1, "description": "range 的 1-based 起始行" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_REQUEST_LINES, "description": "range/head/tail 请求的最大行数" }
                },
                "required": ["path", "mode"],
                "oneOf": [
                    { "properties": { "mode": { "const": "metadata" } }, "not": { "anyOf": [{"required":["line"]}, {"required":["limit"]}] } },
                    { "properties": { "mode": { "const": "range" } }, "required": ["line", "limit"] },
                    { "properties": { "mode": { "const": "head" } }, "required": ["limit"], "not": { "required": ["line"] } },
                    { "properties": { "mode": { "const": "tail" } }, "required": ["limit"], "not": { "required": ["line"] } }
                ]
            }),
            sensitive: true,
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "read_text_file 截止时刻已过".into(),
            });
        }
        let request = ReadRequest::parse(&args)?;
        let path_for_log = request.path.display().to_string();
        let result = tokio::time::timeout_at(ctx.deadline_or_far_future(), read_request(&request))
            .await
            .map_err(|_| CapabilityError::Timeout {
                detail: format!("read_text_file 超时: {path_for_log}"),
            })??;

        if let CapabilityResult::Items { ref items } = result {
            let data = &items[0].data;
            tracing::debug!(
                path = %path_for_log,
                mode = ?request.mode,
                bytes_read = data.get("bytes_read").and_then(|value| value.as_u64()).unwrap_or(0),
                estimated_tokens = data.get("estimated_tokens").and_then(|value| value.as_u64()).unwrap_or(0),
                "read_text_file 完成"
            );
        }
        Ok(result)
    }
}

impl ReadRequest {
    fn parse(args: &Value) -> Result<Self, CapabilityError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| invalid_args("缺少 path 参数"))?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(invalid_args("path 必须是绝对路径"));
        }

        let mode = match args.get("mode").and_then(Value::as_str) {
            Some("metadata") => ReadMode::Metadata,
            Some("range") => ReadMode::Range,
            Some("head") => ReadMode::Head,
            Some("tail") => ReadMode::Tail,
            Some(other) => return Err(invalid_args(&format!("不支持的 mode: {other}"))),
            None => return Err(invalid_args("缺少 mode 参数")),
        };
        let line = parse_positive_u64(args, "line")?;
        let limit = parse_positive_u64(args, "limit")?;
        if limit.is_some_and(|value| value > MAX_REQUEST_LINES) {
            return Err(invalid_args(&format!("limit 不能超过 {MAX_REQUEST_LINES}")));
        }

        match mode {
            ReadMode::Metadata if line.is_some() || limit.is_some() => {
                return Err(invalid_args("metadata 模式禁止传 line/limit"));
            }
            ReadMode::Range if line.is_none() || limit.is_none() => {
                return Err(invalid_args("range 模式必须同时传 line 和 limit"));
            }
            ReadMode::Head | ReadMode::Tail if line.is_some() || limit.is_none() => {
                return Err(invalid_args("head/tail 模式必须传 limit，且禁止传 line"));
            }
            _ => {}
        }
        Ok(Self {
            path,
            mode,
            line,
            limit,
        })
    }
}

fn parse_positive_u64(args: &Value, key: &str) -> Result<Option<u64>, CapabilityError> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_args(&format!("{key} 必须是正整数")))?;
    Ok(Some(value))
}

async fn read_request(request: &ReadRequest) -> Result<CapabilityResult, CapabilityError> {
    let metadata = read_metadata(&request.path).await?;
    if request.mode == ReadMode::Metadata {
        return Ok(single_item(metadata_json(&request.path, &metadata)));
    }
    match metadata.text_status {
        "binary" => return Err(invalid_data("binary", "文件包含 NUL 字节，拒绝按文本读取")),
        "non_utf8" => return Err(invalid_data("non_utf8", "文件探测不是有效 UTF-8")),
        _ => {}
    }

    let file = tokio::fs::File::open(&request.path)
        .await
        .map_err(|error| map_io_error(&request.path, error))?;
    let mut reader = BufReader::with_capacity(8 * 1024, file);
    let page = match request.mode {
        ReadMode::Head => read_forward(&mut reader, 1, request.limit.unwrap()).await?,
        ReadMode::Range => {
            read_forward(&mut reader, request.line.unwrap(), request.limit.unwrap()).await?
        }
        ReadMode::Tail => read_tail(&mut reader, request.limit.unwrap()).await?,
        ReadMode::Metadata => unreachable!(),
    };

    let mut data = metadata_json(&request.path, &metadata);
    let object = data.as_object_mut().expect("metadata is object");
    object.insert("content".into(), json!(page.content));
    object.insert("start_line".into(), json!(page.start_line));
    object.insert("end_line".into(), json!(page.end_line));
    object.insert("eof".into(), json!(page.eof));
    object.insert(
        "truncated_by_budget".into(),
        json!(page.truncated_by_budget),
    );
    object.insert("next_line".into(), json!(page.next_line));
    object.insert("bytes_read".into(), json!(page.bytes_read));
    object.insert("content_bytes".into(), json!(page.content_bytes));
    object.insert("estimated_tokens".into(), json!(page.estimated_tokens));
    Ok(single_item(data))
}

async fn read_metadata(path: &Path) -> Result<TextMetadata, CapabilityError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| map_io_error(path, error))?;
    if !metadata.is_file() {
        return Err(invalid_args("path 必须指向普通文件，不能是目录"));
    }
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_secs()).ok());

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| map_io_error(path, error))?;
    let sample_len = usize::try_from(metadata.len().min(PROBE_BYTES as u64)).unwrap_or(PROBE_BYTES);
    let mut sample = vec![0; sample_len];
    let read = file.read(&mut sample).await.map_err(map_read_error)?;
    sample.truncate(read);
    let without_bom = sample.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&sample);
    let text_status = if sample.is_empty() {
        "empty"
    } else if sample.contains(&0) {
        "binary"
    } else if !is_valid_utf8_probe(without_bom) {
        "non_utf8"
    } else {
        "utf8"
    };
    Ok(TextMetadata {
        size_bytes: metadata.len(),
        modified_at,
        text_status,
    })
}

/// 探测块可能恰好截断一个多字节字符；仅把“确定存在非法字节”的情况判为非 UTF-8。
fn is_valid_utf8_probe(sample: &[u8]) -> bool {
    match std::str::from_utf8(sample) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

async fn read_forward<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    start_line: u64,
    limit: u64,
) -> Result<TextPage, CapabilityError> {
    let mut bytes_read = 0_u64;
    let mut number = 1_u64;
    while number < start_line {
        let Some(bytes) = read_limited_line(reader).await? else {
            return Ok(empty_page(bytes_read));
        };
        bytes_read += bytes.len() as u64;
        decode_line(bytes, number)?;
        number += 1;
    }

    let mut content = String::new();
    let mut content_bytes = 0_usize;
    let mut tokens = 0_usize;
    let mut count = 0_u64;
    let mut end_line = None;
    let mut truncated = false;
    let mut next_line = None;
    let mut reached_eof = false;

    while count < limit {
        let Some(bytes) = read_limited_line(reader).await? else {
            reached_eof = true;
            break;
        };
        bytes_read += bytes.len() as u64;
        let text = decode_line(bytes, number)?;
        let line_bytes = text.len();
        let line_tokens = estimate_tokens(&text);
        if line_tokens > HARD_TOKEN_LIMIT {
            return Err(invalid_data(
                "line_too_long",
                &format!("第 {number} 行估算超过 {HARD_TOKEN_LIMIT} token"),
            ));
        }
        let exceeds_hard = content_bytes + line_bytes > MAX_CONTENT_BYTES
            || tokens + line_tokens > HARD_TOKEN_LIMIT;
        let exceeds_soft = !content.is_empty() && tokens + line_tokens > SOFT_TOKEN_BUDGET;
        if exceeds_hard || exceeds_soft {
            truncated = true;
            next_line = Some(number);
            break;
        }
        content.push_str(&text);
        content_bytes += line_bytes;
        tokens += line_tokens;
        end_line = Some(number);
        number += 1;
        count += 1;
    }

    if !truncated && !reached_eof {
        if reader.fill_buf().await.map_err(map_read_error)?.is_empty() {
            reached_eof = true;
        } else {
            next_line = Some(number);
        }
    }

    Ok(TextPage {
        content,
        start_line: end_line.map(|_| start_line),
        end_line,
        eof: reached_eof,
        truncated_by_budget: truncated,
        next_line,
        bytes_read,
        content_bytes,
        estimated_tokens: tokens,
    })
}

async fn read_tail<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: u64,
) -> Result<TextPage, CapabilityError> {
    let mut lines = VecDeque::with_capacity(limit as usize);
    let mut number = 1_u64;
    let mut bytes_read = 0_u64;
    while let Some(bytes) = read_limited_line(reader).await? {
        bytes_read += bytes.len() as u64;
        let text = decode_line(bytes, number)?;
        let tokens = estimate_tokens(&text);
        if tokens > HARD_TOKEN_LIMIT {
            return Err(invalid_data(
                "line_too_long",
                &format!("第 {number} 行估算超过 {HARD_TOKEN_LIMIT} token"),
            ));
        }
        lines.push_back(BufferedLine {
            number,
            bytes: text.len(),
            tokens,
            text,
        });
        if lines.len() > limit as usize {
            lines.pop_front();
        }
        number += 1;
    }

    let mut selected = Vec::new();
    let mut content_bytes = 0_usize;
    let mut tokens = 0_usize;
    for line in lines.iter().rev() {
        let exceeds_hard = content_bytes + line.bytes > MAX_CONTENT_BYTES
            || tokens + line.tokens > HARD_TOKEN_LIMIT;
        let exceeds_soft = !selected.is_empty() && tokens + line.tokens > SOFT_TOKEN_BUDGET;
        if exceeds_hard || exceeds_soft {
            break;
        }
        content_bytes += line.bytes;
        tokens += line.tokens;
        selected.push(line);
    }
    selected.reverse();
    let truncated = selected.len() < lines.len();
    let content = selected.iter().map(|line| line.text.as_str()).collect();
    Ok(TextPage {
        start_line: selected.first().map(|line| line.number),
        end_line: selected.last().map(|line| line.number),
        content,
        eof: true,
        truncated_by_budget: truncated,
        next_line: None,
        bytes_read,
        content_bytes,
        estimated_tokens: tokens,
    })
}

async fn read_limited_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, CapabilityError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(map_read_error)?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len() + take > MAX_LINE_BYTES {
            return Err(invalid_data(
                "line_too_long",
                &format!("单行超过 {MAX_LINE_BYTES} 字节硬上限"),
            ));
        }
        line.extend_from_slice(&available[..take]);
        let ended = available[take - 1] == b'\n';
        reader.consume(take);
        if ended {
            return Ok(Some(line));
        }
    }
}

fn decode_line(mut bytes: Vec<u8>, line: u64) -> Result<String, CapabilityError> {
    if line == 1 && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.drain(..3);
    }
    if bytes.contains(&0) {
        return Err(invalid_data(
            "binary",
            &format!("第 {line} 行包含 NUL 字节"),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        invalid_data(
            "non_utf8",
            &format!("第 {line} 行不是有效 UTF-8；首版不支持其他编码"),
        )
    })
}

fn empty_page(bytes_read: u64) -> TextPage {
    TextPage {
        content: String::new(),
        start_line: None,
        end_line: None,
        eof: true,
        truncated_by_budget: false,
        next_line: None,
        bytes_read,
        content_bytes: 0,
        estimated_tokens: 0,
    }
}

fn metadata_json(path: &Path, metadata: &TextMetadata) -> Value {
    json!({
        "path": path,
        "file_type": "file",
        "size_bytes": metadata.size_bytes,
        "modified_at": metadata.modified_at,
        "text_status": metadata.text_status,
    })
}

fn single_item(data: Value) -> CapabilityResult {
    CapabilityResult::Items {
        items: vec![ItemResult {
            data,
            desc: Some("文本文件读取结果".into()),
            actions: vec![],
        }],
    }
}

fn invalid_args(detail: &str) -> CapabilityError {
    CapabilityError::InvalidArgs {
        detail: format!("read_text_file: {detail}"),
    }
}

fn invalid_data(reason: &str, detail: &str) -> CapabilityError {
    CapabilityError::InvalidData {
        reason: reason.into(),
        detail: detail.into(),
    }
}

fn map_io_error(path: &Path, error: std::io::Error) -> CapabilityError {
    match error.kind() {
        std::io::ErrorKind::NotFound => CapabilityError::NotFound {
            id: path.display().to_string(),
        },
        std::io::ErrorKind::PermissionDenied => CapabilityError::Permission {
            detail: format!("无法读取 {}: {error}", path.display()),
        },
        _ => CapabilityError::Internal {
            detail: format!("读取 {} 失败: {error}", path.display()),
        },
    }
}

fn map_read_error(error: std::io::Error) -> CapabilityError {
    CapabilityError::Internal {
        detail: format!("读取文本流失败: {error}"),
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ReadTextFile) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn schema_is_sensitive_and_describes_four_modes() {
        let schema = ReadTextFile.schema();
        assert!(schema.sensitive);
        assert_eq!(
            schema.parameters["properties"]["mode"]["enum"],
            json!(["metadata", "range", "head", "tail"])
        );
    }

    #[test]
    fn request_contract_rejects_invalid_combinations() {
        assert!(
            ReadRequest::parse(&json!({"path":"relative.txt","mode":"head","limit":1})).is_err()
        );
        let absolute = std::env::temp_dir().join("blink-contract.txt");
        assert!(ReadRequest::parse(&json!({"path":absolute,"mode":"metadata","limit":1})).is_err());
        assert!(ReadRequest::parse(&json!({"path":absolute,"mode":"range","limit":1})).is_err());
        assert!(
            ReadRequest::parse(&json!({"path":absolute,"mode":"head","line":1,"limit":1})).is_err()
        );
        assert!(ReadRequest::parse(&json!({"path":absolute,"mode":"tail","limit":0})).is_err());
    }

    #[tokio::test]
    async fn head_and_range_return_continuation() {
        let input = Cursor::new(b"one\ntwo\nthree\nfour\n".to_vec());
        let mut reader = BufReader::new(input);
        let head = read_forward(&mut reader, 1, 2).await.unwrap();
        assert_eq!(head.content, "one\ntwo\n");
        assert_eq!(head.next_line, Some(3));
        assert!(!head.eof);

        let input = Cursor::new(b"one\ntwo\nthree\nfour\n".to_vec());
        let mut reader = BufReader::new(input);
        let range = read_forward(&mut reader, 3, 2).await.unwrap();
        assert_eq!(range.content, "three\nfour\n");
        assert_eq!(range.start_line, Some(3));
        assert_eq!(range.end_line, Some(4));
        assert!(range.eof);
    }

    #[tokio::test]
    async fn tail_returns_actual_final_range_without_cursor() {
        let input = Cursor::new(b"one\ntwo\nthree\nfour".to_vec());
        let mut reader = BufReader::new(input);
        let tail = read_tail(&mut reader, 2).await.unwrap();
        assert_eq!(tail.content, "three\nfour");
        assert_eq!(tail.start_line, Some(3));
        assert_eq!(tail.end_line, Some(4));
        assert_eq!(tail.next_line, None);
        assert!(tail.eof);
    }

    #[tokio::test]
    async fn oversized_single_line_fails_without_unbounded_buffer() {
        let input = Cursor::new(vec![b'a'; MAX_LINE_BYTES + 1]);
        let mut reader = BufReader::with_capacity(1024, input);
        let error = read_limited_line(&mut reader).await.unwrap_err();
        assert!(matches!(
            error,
            CapabilityError::InvalidData { ref reason, .. } if reason == "line_too_long"
        ));
    }

    #[test]
    fn bom_is_removed_and_invalid_utf8_is_rejected() {
        assert_eq!(
            decode_line(vec![0xEF, 0xBB, 0xBF, b'a', b'\n'], 1).unwrap(),
            "a\n"
        );
        assert!(matches!(
            decode_line(vec![0xFF], 1),
            Err(CapabilityError::InvalidData { ref reason, .. }) if reason == "non_utf8"
        ));
    }

    #[tokio::test]
    async fn budget_truncation_preserves_next_line_and_hard_limits() {
        let line = format!("{}\n", "word ".repeat(1_000));
        let input = Cursor::new(format!("{line}{line}{line}").into_bytes());
        let mut reader = BufReader::new(input);
        let page = read_forward(&mut reader, 1, 3).await.unwrap();
        assert!(page.truncated_by_budget);
        assert_eq!(page.next_line, Some(2));
        assert!(page.estimated_tokens <= HARD_TOKEN_LIMIT);
        assert!(page.content_bytes <= MAX_CONTENT_BYTES);
    }

    #[tokio::test]
    async fn metadata_reports_stats_without_content_and_detects_binary() {
        let unique = format!(
            "blink-read-text-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        tokio::fs::write(&path, [b'a', 0, b'b']).await.unwrap();

        let request = ReadRequest {
            path: path.clone(),
            mode: ReadMode::Metadata,
            line: None,
            limit: None,
        };
        let result = read_request(&request).await.unwrap();
        let CapabilityResult::Items { items } = result else {
            panic!("expected items")
        };
        assert_eq!(items[0].data["size_bytes"], 3);
        assert_eq!(items[0].data["text_status"], "binary");
        assert!(items[0].data.get("content").is_none());

        tokio::fs::remove_file(path).await.unwrap();
    }

    #[test]
    fn utf8_probe_accepts_incomplete_multibyte_suffix_only() {
        assert!(is_valid_utf8_probe(&[b'a', 0xE4, 0xBD]));
        assert!(!is_valid_utf8_probe(&[b'a', 0xFF]));
    }
}
