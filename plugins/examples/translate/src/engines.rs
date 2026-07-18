//! 5 个翻译引擎实现（1:1 迁移自 Python main.py）。
//!
//! 签名算法各引擎不同：
//! - youdao: SHA256(appKey + text + salt + curtime + appSecret)
//! - baidu: MD5(appid + text + salt + key)
//! - deepl: 无签名（auth_key 参数）
//! - ali: HMAC-SHA1 签名（排序参数 + URL 编码 + base64）
//! - tencent: TC3-HMAC-SHA256 签名（4 步签名链）

use serde_json::{Value, json};

use crate::engine::{EngineRequest, TranslateEngine};

// ── 语言代码映射 ──────────────────────────────────────────────────────────────

/// youdao/baidu/deepl 语言映射（1:1 对齐 Python LANG_MAP）
fn lang_map(engine: &str, target_lang: &str) -> &'static str {
    match (engine, target_lang) {
        ("youdao", "zh") => "zh-CHS",
        ("youdao", "en") => "en",
        ("youdao", "ja") => "ja",
        ("youdao", "ko") => "ko",
        ("baidu", "zh") => "zh",
        ("baidu", "en") => "en",
        ("baidu", "ja") => "jp",
        ("baidu", "ko") => "kor",
        ("deepl", "zh") => "ZH",
        ("deepl", "en") => "EN",
        ("deepl", "ja") => "JA",
        ("deepl", "ko") => "KO",
        _ => "zh",
    }
}

/// ali/tencent 语言映射（直映射，1:1 对齐 Python）
fn lang_direct(target_lang: &str) -> &'static str {
    match target_lang {
        "zh" => "zh",
        "en" => "en",
        "ja" => "ja",
        "ko" => "ko",
        _ => "zh",
    }
}

// ── 有道智云 ──────────────────────────────────────────────────────────────────

pub struct YoudaoEngine;

impl TranslateEngine for YoudaoEngine {
    fn build_request(&self, text: &str, target_lang: &str, settings: &Value) -> Option<EngineRequest> {
        let app_key = settings.get("youdao_app_key").and_then(Value::as_str).unwrap_or("");
        let app_secret = settings.get("youdao_app_secret").and_then(Value::as_str).unwrap_or("");
        if app_key.is_empty() || app_secret.is_empty() {
            return None;
        }

        let salt: u32 = 10000 + chrono::Local::now().timestamp_subsec_nanos() % 90000;
        let curtime = chrono::Utc::now().timestamp();
        let sign_str = format!("{app_key}{text}{salt}{curtime}{app_secret}");
        let sign = hex_hash::sha256_hex(&sign_str);
        let tgt = lang_map("youdao", target_lang);

        let params = json!({
            "q": text,
            "from": "auto",
            "to": tgt,
            "appKey": app_key,
            "salt": salt.to_string(),
            "sign": sign,
            "signType": "v3",
            "curtime": curtime.to_string(),
        });

        let body = serde_urlencoded_encode(&params);
        Some(EngineRequest {
            method: "POST".into(),
            url: "https://openapi.youdao.com/api".into(),
            body: Some(body),
            timeout_ms: 8000,
            headers: vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())],
        })
    }

    fn parse_response(&self, body: &str) -> Option<String> {
        let v: Value = serde_json::from_str(body).ok()?;
        v.get("translation")?.get(0)?.as_str().map(|s| s.to_string())
    }
}

// ── 百度翻译 ──────────────────────────────────────────────────────────────────

pub struct BaiduEngine;

impl TranslateEngine for BaiduEngine {
    fn build_request(&self, text: &str, target_lang: &str, settings: &Value) -> Option<EngineRequest> {
        let app_id = settings.get("baidu_app_id").and_then(Value::as_str).unwrap_or("");
        let app_key = settings.get("baidu_app_key").and_then(Value::as_str).unwrap_or("");
        if app_id.is_empty() || app_key.is_empty() {
            return None;
        }

        let salt: u32 = 10000 + chrono::Local::now().timestamp_subsec_nanos() % 90000;
        let sign_str = format!("{app_id}{text}{salt}{app_key}");
        let sign = hex_hash::md5_hex(&sign_str);
        let tgt = lang_map("baidu", target_lang);

        let params = json!({
            "q": text,
            "from": "auto",
            "to": tgt,
            "appid": app_id,
            "salt": salt.to_string(),
            "sign": sign,
        });
        let query = serde_urlencoded_encode(&params);

        Some(EngineRequest {
            method: "GET".into(),
            url: format!("https://fanyi-api.baidu.com/api/trans/vip/translate?{query}"),
            body: None,
            timeout_ms: 8000,
            headers: vec![],
        })
    }

    fn parse_response(&self, body: &str) -> Option<String> {
        let v: Value = serde_json::from_str(body).ok()?;
        let arr = v.get("trans_result")?.as_array()?;
        let texts: Vec<String> = arr.iter().filter_map(|item| {
            item.get("dst").and_then(|d| d.as_str()).map(|s| s.to_string())
        }).collect();
        if texts.is_empty() { None } else { Some(texts.join("\n")) }
    }
}

// ── DeepL ─────────────────────────────────────────────────────────────────────

pub struct DeeplEngine;

impl TranslateEngine for DeeplEngine {
    fn build_request(&self, _text: &str, target_lang: &str, settings: &Value) -> Option<EngineRequest> {
        let api_key = settings.get("deepl_api_key").and_then(Value::as_str).unwrap_or("");
        if api_key.is_empty() {
            return None;
        }

        let tgt = lang_map("deepl", target_lang);
        let params = json!({
            "text": _text,
            "target_lang": tgt,
            "auth_key": api_key,
        });
        let body = serde_urlencoded_encode(&params);

        Some(EngineRequest {
            method: "POST".into(),
            url: "https://api-free.deepl.com/v2/translate".into(),
            body: Some(body),
            timeout_ms: 8000,
            headers: vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())],
        })
    }

    fn parse_response(&self, body: &str) -> Option<String> {
        let v: Value = serde_json::from_str(body).ok()?;
        v.get("translations")?.get(0)?.get("text")?.as_str().map(|s| s.to_string())
    }
}

// ── 阿里机器翻译（HMAC-SHA1）──────────────────────────────────────────────────

pub struct AliEngine;

impl TranslateEngine for AliEngine {
    fn build_request(&self, text: &str, target_lang: &str, settings: &Value) -> Option<EngineRequest> {
        let access_key_id = settings.get("ali_access_key_id").and_then(Value::as_str).unwrap_or("");
        let access_key_secret = settings.get("ali_access_key_secret").and_then(Value::as_str).unwrap_or("");
        if access_key_id.is_empty() || access_key_secret.is_empty() {
            return None;
        }

        let tgt = lang_direct(target_lang);
        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let nonce = uuid_like();
        let mut params = vec![
            ("Format", "JSON".into()),
            ("Version", "2018-10-12".into()),
            ("AccessKeyId", access_key_id.to_string()),
            ("SignatureMethod", "HMAC-SHA1".into()),
            ("Timestamp", timestamp),
            ("SignatureVersion", "1.0".into()),
            ("SignatureNonce", nonce),
            ("Action", "TranslateGeneral".into()),
            ("SourceLanguage", "auto".into()),
            ("TargetLanguage", tgt.to_string()),
            ("SourceText", text.to_string()),
            ("FormatType", "text".into()),
            ("Scene", "general".into()),
        ];
        params.sort_by(|a, b| a.0.cmp(b.0));

        // canonicalized: k=v 用 quote 编码（safe 为空）
        let canonicalized: String = params.iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>().join("&");

        // 签名字符串：POST&%2F&<quote(canonicalized)>
        let string_to_sign = format!("POST&%2F&{}", urlencoding::encode(&canonicalized));
        let signing_key = format!("{access_key_secret}&");
        let signature = hex_hash::hmac_sha1_b64(signing_key.as_bytes(), string_to_sign.as_bytes());

        // 构造最终 body
        let mut body_params = params;
        body_params.push(("Signature", signature));
        let body = body_params.iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>().join("&");

        Some(EngineRequest {
            method: "POST".into(),
            url: "https://mt.aliyuncs.com/".into(),
            body: Some(body),
            timeout_ms: 8000,
            headers: vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())],
        })
    }

    fn parse_response(&self, body: &str) -> Option<String> {
        let v: Value = serde_json::from_str(body).ok()?;
        v.get("Data")?.get("Translated")?.as_str().map(|s| s.to_string())
    }
}

// ── 腾讯云机器翻译（TC3-HMAC-SHA256）──────────────────────────────────────────

pub struct TencentEngine;

impl TranslateEngine for TencentEngine {
    fn build_request(&self, text: &str, target_lang: &str, settings: &Value) -> Option<EngineRequest> {
        let secret_id = settings.get("tencent_secret_id").and_then(Value::as_str).unwrap_or("");
        let secret_key = settings.get("tencent_secret_key").and_then(Value::as_str).unwrap_or("");
        if secret_id.is_empty() || secret_key.is_empty() {
            return None;
        }

        let service = "tmt";
        let host = "tmt.tencentcloudapi.com";
        let action = "TextTranslate";
        let version = "2018-03-21";

        let tgt = lang_direct(target_lang);
        let payload = json!({
            "SourceText": text,
            "Source": "auto",
            "Target": tgt,
            "ProjectId": 0
        }).to_string();

        let timestamp = chrono::Utc::now().timestamp();
        let date = chrono::DateTime::from_timestamp(timestamp, 0)
            .unwrap_or_else(|| chrono::Utc::now())
            .format("%Y-%m-%d").to_string();

        // Step 1: 规范请求
        let content_type = "application/json; charset=utf-8";
        let canonical_headers = format!("content-type:{content_type}\nhost:{host}\nx-tc-action:{}\n", action.to_lowercase());
        let signed_headers = "content-type;host;x-tc-action";
        let hashed_payload = hex_hash::sha256_hex(&payload);
        let canonical_request = format!(
            "POST\n/\n\n{canonical_headers}\n{signed_headers}\n{hashed_payload}"
        );

        // Step 2: 拼接待签名字符串
        let algorithm = "TC3-HMAC-SHA256";
        let credential_scope = format!("{date}/{service}/tc3_request");
        let hashed_canonical = hex_hash::sha256_hex(&canonical_request);
        let string_to_sign = format!("{algorithm}\n{timestamp}\n{credential_scope}\n{hashed_canonical}");

        // Step 3: 计算签名（4 步 HMAC 链）
        let secret_date = hex_hash::hmac_sha256_raw(format!("TC3{secret_key}").as_bytes(), date.as_bytes());
        let secret_service = hex_hash::hmac_sha256_raw(&secret_date, service.as_bytes());
        let secret_signing = hex_hash::hmac_sha256_raw(&secret_service, b"tc3_request");
        let signature = hex_hash::hmac_sha256_hex(&secret_signing, string_to_sign.as_bytes());

        // Step 4: Authorization header
        let authorization = format!(
            "{algorithm} Credential={secret_id}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
        );

        Some(EngineRequest {
            method: "POST".into(),
            url: format!("https://{host}"),
            body: Some(payload),
            timeout_ms: 8000,
            headers: vec![
                ("Authorization".into(), authorization),
                ("Content-Type".into(), content_type.to_string()),
                ("Host".into(), host.to_string()),
                ("X-TC-Action".into(), action.to_string()),
                ("X-TC-Timestamp".into(), timestamp.to_string()),
                ("X-TC-Version".into(), version.to_string()),
                ("X-TC-Region".into(), "ap-guangzhou".to_string()),
            ],
        })
    }

    fn parse_response(&self, body: &str) -> Option<String> {
        let v: Value = serde_json::from_str(body).ok()?;
        v.get("Response")?.get("TargetText")?.as_str().map(|s| s.to_string())
    }
}

// ── 哈希/HMAC 辅助（避免重复依赖样板）────────────────────────────────────────

/// 统一的哈希辅助模块——封装 sha2/sha1/md5/hmac 的样板。
mod hex_hash {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    pub fn sha256_hex(data: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data.as_bytes());
        hex_encode(&hasher.finalize())
    }

    pub fn md5_hex(data: &str) -> String {
        use md5::{Md5, Digest};
        let mut hasher = Md5::new();
        hasher.update(data.as_bytes());
        hex_encode(&hasher.finalize())
    }

    /// HMAC-SHA1 → base64（阿里签名用）
    pub fn hmac_sha1_b64(key: &[u8], data: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        use sha1::Sha1;
        type HmacSha1 = Hmac<Sha1>;
        let mut mac = HmacSha1::new_from_slice(key).expect("HMAC key error");
        mac.update(data);
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    /// HMAC-SHA256 → 原始字节（腾讯签名链中间步用）
    pub fn hmac_sha256_raw(key: &[u8], data: &[u8]) -> Vec<u8> {
        use hmac::{Hmac, Mac};
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key error");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    /// HMAC-SHA256 → hex（腾讯签名最终步用）
    pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
        hex_encode(&hmac_sha256_raw(key, data))
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

// ── URL 编码辅助 ───────────────────────────────────────────────────────────────

/// 把 serde_json::Value（object）编码成 application/x-www-form-urlencoded 格式。
fn serde_urlencoded_encode(params: &Value) -> String {
    let obj = match params.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    obj.iter()
        .map(|(k, v)| {
            let val = v.as_str().unwrap_or("");
            format!("{}={}", urlencoding::encode(k), urlencoding::encode(val))
        })
        .collect::<Vec<_>>().join("&")
}

/// 生成类 UUID 的 nonce（不用 uuid crate，用时间戳+随机数模拟）。
fn uuid_like() -> String {
    let now = chrono::Utc::now();
    format!("{}-{:x}", now.timestamp(), now.timestamp_subsec_nanos())
}
