//! 词库数据集构建器（0.28.4 §五.2）——构建期完成裁剪与索引，产出不可变
//! 数据包（dataset.json + dict.db / ety.db），不让用户首次查询解析全量 CSV。
//!
//! **产物契约**（与宿主 `domain/plugin/dataset.rs::DatasetManifest` 对齐）：
//! - 基础包 `dictionary-basic`：entries（词头/义项/词形/短语）+ forms（变形→原型）
//!   + zh_index（中文反查，仅频率信号条目）；
//! - 词源包 `dictionary-etymology`（可选增强）：ety(word → roots[])；
//! - dataset.json 携带 files sha256（安装完整性校验）与 content_sha256
//!   （构建内容摘要 = DB 行流的规范序哈希，重建可复验）。
//!
//! **裁剪剖面**（phase：构建期裁剪，不是运行期过滤）：
//! - basic：全部单词（技术词不受语料频率限制）+ 信号短语（bnc/frq/tag/collins/
//!   oxford 任一）+ ≤4 个小写字母 token 的常见短语；
//! - full：全部有译/义项条目。
//!
//! **汉英反查边界**：zh_index 只为带频率信号的条目建索引（约 1/13 词条）——
//! 基础反查覆盖常用概念；长尾词条的中文反查不在本剖面承诺内。

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;


/// 数据集 dataset.json 的格式版本（与宿主 SUPPORTED_DATASET_FORMAT_VERSION 对齐）。
pub const FORMAT_VERSION: u32 = 1;
/// 基础词库数据集 id。
pub const BASIC_DATASET_ID: &str = "dictionary-basic";
/// 词源增强包 id。
pub const ETY_DATASET_ID: &str = "dictionary-etymology";
/// 数据包归属插件。
pub const PLUGIN_ID: &str = "builtin.dictionary";
/// ECDICT 源信息（许可可追溯，DATASETS.md 有完整记录）。
pub const ECDICT_SOURCE: (&str, &str, &str) =
    ("ECDICT", "https://github.com/skywind3000/ECDICT", "MIT");

/// 无频率信号条目的排序频率（rank 语义，越大越冷门）。
const FREQ_UNKNOWN: i64 = 999_999_999;

/// 构建剖面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Basic,
    Full,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "basic" => Some(Profile::Basic),
            "full" => Some(Profile::Full),
            _ => None,
        }
    }

    fn wire_name(self) -> &'static str {
        match self {
            Profile::Basic => "basic",
            Profile::Full => "full",
        }
    }
}

/// 构建统计（记入 dataset.json counts 与构建日志）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct BuildStats {
    pub entries: u64,
    pub phrases: u64,
    pub forms: u64,
    pub zh_terms: u64,
    /// 构建剖面读入的源行数（含被裁剪的）。
    pub source_rows: u64,
}

/// 构建选项。
#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub profile: Profile,
    /// 数据集版本（目录名，缺省 "1.0.0"）。
    pub version: String,
    /// 来源修订标记（commit/日期；溯源用）。
    pub source_revision: Option<String>,
    /// 展示名。
    pub name: String,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            profile: Profile::Basic,
            version: "1.0.0".into(),
            source_revision: None,
            name: "Blink 离线词库（ECDICT）".into(),
        }
    }
}

/// 构建结果：版本目录（dataset.json + 载荷文件）。
#[derive(Debug, Clone)]
pub struct BuildOutput {
    /// 数据包目录（含 dataset.json 与载荷；安装时直接指向此目录或其 zip）。
    pub package_dir: PathBuf,
    pub stats: BuildStats,
    pub content_sha256: String,
}

/// 构建基础词库：ECDICT CSV → `<out_root>/<dataset_id>/<version>/{dataset.json,dict.db}`。
pub fn build_basic_dataset(
    csv_path: &Path,
    opts: &BuildOptions,
    out_root: &Path,
) -> Result<BuildOutput, String> {
    let package_dir = out_root.join(BASIC_DATASET_ID).join(&opts.version);
    if package_dir.exists() {
        std::fs::remove_dir_all(&package_dir).map_err(|e| format!("清理旧产物失败: {e}"))?;
    }
    std::fs::create_dir_all(&package_dir).map_err(|e| format!("创建产物目录失败: {e}"))?;
    let db_path = package_dir.join("dict.db");

    let mut stats = BuildStats::default();
    let conn = Connection::open(&db_path).map_err(|e| format!("创建 dict.db 失败: {e}"))?;
    create_basic_schema(&conn)?;
    build_entries_and_indexes(&conn, csv_path, opts.profile, &mut stats)?;
    let content_sha256 = compute_basic_digest(&conn);
    write_meta(&conn, BASIC_DATASET_ID, &content_sha256, &stats)?;
    conn.pragma_update(None, "journal_mode", "DELETE").map_err(|e| e.to_string())?;
    conn.execute("VACUUM", []).map_err(|e| format!("VACUUM 失败: {e}"))?;
    drop(conn);

    write_dataset_json(
        &package_dir,
        &[("dict.db", &db_path)],
        BASIC_DATASET_ID,
        &content_sha256,
        opts,
        &[("entries", stats.entries), ("phrases", stats.phrases), ("forms", stats.forms), ("zh_terms", stats.zh_terms)],
    )?;

    eprintln!(
        "[dictionary-build] 基础词库完成：{} 词条（短语 {}）/ {} 词形 / {} 中文反查键 → {}",
        stats.entries, stats.phrases, stats.forms, stats.zh_terms,
        db_path.display()
    );
    Ok(BuildOutput {
        package_dir,
        stats,
        content_sha256,
    })
}

/// 构建词源增强包：wordroot.txt（JSON）→ `<out_root>/<dataset_id>/<version>/{dataset.json,ety.db}`。
pub fn build_etymology_dataset(
    wordroot_json: &Path,
    version: &str,
    out_root: &Path,
) -> Result<BuildOutput, String> {
    let package_dir = out_root.join(ETY_DATASET_ID).join(version);
    if package_dir.exists() {
        std::fs::remove_dir_all(&package_dir).map_err(|e| format!("清理旧产物失败: {e}"))?;
    }
    std::fs::create_dir_all(&package_dir).map_err(|e| format!("创建产物目录失败: {e}"))?;
    let db_path = package_dir.join("ety.db");

    let raw = std::fs::read_to_string(wordroot_json)
        .map_err(|e| format!("读取 wordroot 失败: {e}"))?;
    let roots: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw)
        .map_err(|e| format!("wordroot JSON 解析失败: {e}"))?;

    let conn = Connection::open(&db_path).map_err(|e| format!("创建 ety.db 失败: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE ety(word TEXT PRIMARY KEY, roots TEXT NOT NULL);",
    )
    .map_err(|e| e.to_string())?;

    let mut stats = BuildStats::default();
    {
        let mut select_ety = conn
            .prepare("SELECT roots FROM ety WHERE word = ?1")
            .map_err(|e| e.to_string())?;
        let mut insert_ety = conn
            .prepare("INSERT OR REPLACE INTO ety(word, roots) VALUES(?1, ?2)")
            .map_err(|e| e.to_string())?;
        conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;
        for (root, info) in &roots {
            let examples = match info.get("example").and_then(|v| v.as_array()) {
                Some(list) => list,
                None => continue,
            };
            let entry = serde_json::json!({
                "root": root,
                "meaning": info.get("meaning").and_then(|v| v.as_str()).unwrap_or(""),
                "class": info.get("class").and_then(|v| v.as_str()).unwrap_or(""),
                "origin": info.get("origin").and_then(|v| v.as_str()).unwrap_or(""),
            });
            for word in examples.iter().filter_map(|v| v.as_str()) {
                let word = word.trim().to_lowercase();
                if word.is_empty() || word.contains(char::is_whitespace) {
                    continue;
                }
                // 同词多词根：读取既有条目，把新词根合并进 JSON 数组。
                let existing: Option<String> =
                    select_ety.query_row([&word], |r| r.get(0)).ok();
                let combined = match existing {
                    Some(prev) => {
                        let mut arr: Vec<serde_json::Value> =
                            serde_json::from_str(&prev).unwrap_or_default();
                        arr.push(entry.clone());
                        serde_json::to_string(&arr).map_err(|e| e.to_string())?
                    }
                    None => format!("[{entry}]"),
                };
                insert_ety
                    .execute(rusqlite::params![word, combined])
                    .map_err(|e| e.to_string())?;
                stats.forms += 1; // 复用字段记 (word→root) 映射数
            }
            stats.entries += 1; // 复用字段记词根条目数
        }
        conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
    }

    let content_sha256 = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        let mut stmt = conn
            .prepare("SELECT word, roots FROM ety ORDER BY word")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let word: String = row.get(0).map_err(|e| e.to_string())?;
            let roots: String = row.get(1).map_err(|e| e.to_string())?;
            hasher.update(word.as_bytes());
            hasher.update(&[0x1f]);
            hasher.update(roots.as_bytes());
            hasher.update(&[0x1e]);
        }
        format!("{:x}", hasher.finalize())
    };
    conn.execute_batch(&format!(
        "INSERT OR REPLACE INTO meta VALUES('format_version','{FORMAT_VERSION}');
         INSERT OR REPLACE INTO meta VALUES('dataset_id','{ETY_DATASET_ID}');
         INSERT OR REPLACE INTO meta VALUES('content_sha256','{content_sha256}');"
    ))
    .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "journal_mode", "DELETE").map_err(|e| e.to_string())?;
    conn.execute("VACUUM", []).map_err(|e| e.to_string())?;
    drop(conn);

    let opts = BuildOptions {
        profile: Profile::Basic,
        version: version.to_string(),
        source_revision: None,
        name: "Blink 词源包（ECDICT wordroot）".into(),
    };
    write_dataset_json(
        &package_dir,
        &[("ety.db", &db_path)],
        ETY_DATASET_ID,
        &content_sha256,
        &opts,
        &[("roots", stats.entries), ("words", stats.forms)],
    )?;
    eprintln!(
        "[dictionary-build] 词源包完成：{} 词根 / {} 词条 → {}",
        stats.entries,
        stats.forms,
        db_path.display()
    );
    Ok(BuildOutput {
        package_dir,
        stats,
        content_sha256,
    })
}

/// 校验数据包：files sha256 + content 摘要（与 dataset.json 声明比对）。
/// 返回 (ok, 检查明细)。--verify 与安装校验共用。
pub fn verify_dataset(package_dir: &Path) -> Result<bool, String> {
    let manifest_path = package_dir.join("dataset.json");
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).map_err(|e| format!("读取 dataset.json 失败: {e}"))?,
    )
    .map_err(|e| format!("dataset.json 解析失败: {e}"))?;
    let dataset_id = manifest["dataset_id"].as_str().unwrap_or_default().to_string();

    // 1. files sha256
    if let Some(files) = manifest["files"].as_object() {
        for (name, entry) in files {
            let path = package_dir.join(name);
            let meta = std::fs::metadata(&path).map_err(|e| format!("包内文件缺失 {name}: {e}"))?;
            let expect_bytes = entry["bytes"].as_u64().unwrap_or(u64::MAX);
            if meta.len() != expect_bytes {
                return Ok(false);
            }
            let sha = sha256_file(&path)?;
            if !sha.eq_ignore_ascii_case(entry["sha256"].as_str().unwrap_or("")) {
                return Ok(false);
            }
        }
    } else {
        return Ok(false);
    }

    // 2. content 摘要（按数据集类型重算行流哈希）
    let expect = manifest["content_sha256"].as_str().unwrap_or("");
    let actual = match dataset_id.as_str() {
        BASIC_DATASET_ID => {
            let conn = open_readonly(&package_dir.join("dict.db"))?;
            compute_basic_digest(&conn)
        }
        ETY_DATASET_ID => {
            let conn = open_readonly(&package_dir.join("ety.db"))?;
            compute_ety_digest(&conn)
        }
        other => return Err(format!("未知数据集 id: {other}")),
    };
    Ok(actual == expect)
}

/// 质量样本校验（构建后立即跑，phase §五.4.1 固定公开样本与阈值）。
/// 样本文件见 quality-samples.json；任一不达标 → Err（带明细）。
pub fn check_quality_samples(package_dir: &Path, samples: &Path) -> Result<(), String> {
    let spec: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(samples).map_err(|e| format!("读取样本文件失败: {e}"))?,
    )
    .map_err(|e| format!("样本 JSON 解析失败: {e}"))?;
    let conn = open_readonly(&package_dir.join("dict.db"))?;
    let mut failures: Vec<String> = Vec::new();

    let mut check_exact = |word: &str, expect: Option<&str>| {
        let translation: Option<String> = conn
            .query_row("SELECT translation FROM entries WHERE word = ?1", [word], |r| r.get(0))
            .ok();
        match translation {
            Some(t) if expect.is_none_or(|e| t.contains(e)) => {}
            Some(_) => failures.push(format!("exact {word:?} 释义不含预期片段")),
            None => failures.push(format!("exact {word:?} 未收录")),
        }
    };

    for case in spec["exact"].as_array().unwrap_or(&vec![]).iter().chain(spec["multi_sense"].as_array().unwrap_or(&vec![]).iter()).chain(spec["tech"].as_array().unwrap_or(&vec![]).iter()).chain(spec["phrase"].as_array().unwrap_or(&vec![]).iter()) {
        let word = case["word"].as_str().unwrap_or_default();
        let expect = case["expect_translation_contains"].as_str();
        check_exact(word, expect);
    }

    // 多义：translation 至少 N 个义项行
    for case in spec["multi_sense"].as_array().unwrap_or(&vec![]).iter() {
        let word = case["word"].as_str().unwrap_or_default();
        let min_lines = case["min_sense_lines"].as_u64().unwrap_or(2) as usize;
        if let Some(t) = conn
            .query_row("SELECT translation FROM entries WHERE word = ?1", [word], |r| {
                r.get::<_, String>(0)
            })
            .ok()
        {
            let lines = t.split("\\n").filter(|l| !l.trim().is_empty()).count();
            if lines < min_lines {
                failures.push(format!("multi_sense {word:?} 义项行 {lines} < {min_lines}"));
            }
        }
    }

    // 词形：form → base
    for case in spec["form"].as_array().unwrap_or(&vec![]).iter() {
        let form = case["form"].as_str().unwrap_or_default();
        let base = case["base"].as_str().unwrap_or_default();
        let hit: Option<String> = conn
            .query_row(
                "SELECT e.word FROM forms f JOIN entries e ON e.id = f.entry_id WHERE f.form = ?1",
                [form],
                |r| r.get(0),
            )
            .ok();
        match hit {
            Some(w) if w == base => {}
            Some(w) => failures.push(format!("form {form:?} 还原到 {w:?}（预期 {base:?}）")),
            None => failures.push(format!("form {form:?} 无词形记录")),
        }
    }

    // 汉英反查（与运行时 reverse_zh 同排序：频率优先，否则检查按插入序
    // 命中冷僻同义词）
    for case in spec["reverse_zh"].as_array().unwrap_or(&vec![]).iter() {
        let term = case["term"].as_str().unwrap_or_default();
        let expect_word = case["expect_contains_word"].as_str().unwrap_or_default();
        let hit: Option<String> = conn
            .query_row(
                "SELECT e.word FROM zh_index z JOIN entries e ON e.id = z.entry_id
                 WHERE z.term = ?1 ORDER BY e.freq LIMIT 1",
                [term],
                |r| r.get(0),
            )
            .ok();
        match hit {
            Some(w) if w.contains(expect_word) => {}
            Some(w) => failures.push(format!("reverse {term:?} 命中 {w:?}（预期含 {expect_word:?}）")),
            None => failures.push(format!("reverse {term:?} 无反查记录")),
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("质量样本未达标：\n  - {}", failures.join("\n  - ")))
    }
}

// ── 内部实现 ────────────────────────────────────────────────────────────────

fn create_basic_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE entries(
           id INTEGER PRIMARY KEY,
           word TEXT NOT NULL,
           display TEXT NOT NULL,
           phonetic TEXT NOT NULL DEFAULT '',
           definition TEXT NOT NULL DEFAULT '',
           translation TEXT NOT NULL DEFAULT '',
           pos TEXT NOT NULL DEFAULT '',
           tag TEXT NOT NULL DEFAULT '',
           collins INTEGER NOT NULL DEFAULT 0,
           oxford INTEGER NOT NULL DEFAULT 0,
           freq INTEGER NOT NULL DEFAULT 999999999,
           is_phrase INTEGER NOT NULL DEFAULT 0
         );
         CREATE UNIQUE INDEX idx_entries_word ON entries(word);
         CREATE TABLE forms(form TEXT NOT NULL, entry_id INTEGER NOT NULL, kind TEXT NOT NULL DEFAULT '', PRIMARY KEY(form, entry_id)) WITHOUT ROWID;
         CREATE TABLE zh_index(term TEXT NOT NULL, entry_id INTEGER NOT NULL, PRIMARY KEY(term, entry_id)) WITHOUT ROWID;",
    )
    .map_err(|e| format!("建表失败: {e}"))
}

fn build_entries_and_indexes(
    conn: &Connection,
    csv_path: &Path,
    profile: Profile,
    stats: &mut BuildStats,
) -> Result<(), String> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_path(csv_path)
        .map_err(|e| format!("打开 CSV 失败: {e}"))?;

    let mut insert_entry = conn
        .prepare(
            "INSERT OR IGNORE INTO entries(word, display, phonetic, definition, translation, pos, tag, collins, oxford, freq, is_phrase)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        )
        .map_err(|e| e.to_string())?;
    let mut insert_form = conn
        .prepare("INSERT OR IGNORE INTO forms(form, entry_id, kind) VALUES(?1,?2,?3)")
        .map_err(|e| e.to_string())?;
    let mut insert_zh = conn
        .prepare("INSERT OR IGNORE INTO zh_index(term, entry_id) VALUES(?1,?2)")
        .map_err(|e| e.to_string())?;

    let header = reader
        .headers()
        .map_err(|e| format!("读取 CSV 头失败: {e}"))?
        .clone();
    let col = |name: &str| header.iter().position(|h| h == name);

    let (i_word, i_phon, i_def, i_trans, i_pos, i_collins, i_oxford, i_tag, i_bnc, i_frq, i_ex) = (
        col("word").ok_or("CSV 缺 word 列")?,
        col("phonetic").unwrap_or(1),
        col("definition").unwrap_or(2),
        col("translation").unwrap_or(3),
        col("pos").unwrap_or(4),
        col("collins").unwrap_or(5),
        col("oxford").unwrap_or(6),
        col("tag").unwrap_or(7),
        col("bnc").unwrap_or(8),
        col("frq").unwrap_or(9),
        col("exchange").unwrap_or(10),
    );

    conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;
    let mut txn_rows: u64 = 0;
    // 延迟登记的 lemma 映射（本词 → 原型词）：原型行可能尚未插入（如 am→be，
    // 字母序在原型之前），全量 entry 落库后统一回填。
    let mut pending_lemmas: Vec<(String, String, String)> = Vec::new();

    for record in reader.records() {
        let record = record.map_err(|e| format!("CSV 行解析失败: {e}"))?;
        stats.source_rows += 1;
        let get = |idx: usize| record.get(idx).map(str::trim).unwrap_or("");
        let word = get(i_word);
        if word.is_empty() || word.len() > 128 {
            continue;
        }
        let translation = get(i_trans);
        let definition = get(i_def);
        if translation.is_empty() && definition.is_empty() {
            continue;
        }
        let bnc = get(i_bnc).parse::<i64>().unwrap_or(0);
        let frq = get(i_frq).parse::<i64>().unwrap_or(0);
        let collins = get(i_collins).parse::<i64>().unwrap_or(0);
        let oxford = if get(i_oxford) == "1" { 1 } else { 0 };
        let tag = get(i_tag);
        let has_signal = bnc > 0 || frq > 0 || !tag.is_empty() || collins > 0 || oxford == 1;
        let is_phrase = word.contains(char::is_whitespace);

        if !keep_entry(profile, word, is_phrase, has_signal) {
            continue;
        }

        let display = word.to_string();
        let key = word.trim().to_lowercase();
        let freq = [bnc, frq]
            .iter()
            .copied()
            .filter(|r| *r > 0)
            .min()
            .unwrap_or(FREQ_UNKNOWN);
        let inserted = insert_entry
            .execute(rusqlite::params![
                key,
                display,
                get(i_phon),
                definition,
                translation,
                get(i_pos),
                tag,
                collins,
                oxford,
                freq,
                is_phrase as i64
            ])
            .map_err(|e| e.to_string())?;
        if inserted == 0 {
            continue; // 重复词头（源序在先者保留）
        }
        let entry_id = conn.last_insert_rowid();
        stats.entries += 1;
        if is_phrase {
            stats.phrases += 1;
        }
        // 词形（本词的变形 → 本词）：p/d/i/3/r/t/s
        let exchange = get(i_ex);
        if !exchange.is_empty() {
            let lemma_note = exchange_field(exchange, "1");
            for seg in exchange.split('/') {
                let Some((kind, value)) = seg.split_once(':') else {
                    continue;
                };
                let value = value.trim().to_lowercase();
                if value.is_empty() || value == key {
                    continue;
                }
                match kind {
                    "p" | "d" | "i" | "3" | "r" | "t" | "s" => {
                        if insert_form
                            .execute(rusqlite::params![value, entry_id, kind])
                            .map_err(|e| e.to_string())?
                            > 0
                        {
                            stats.forms += 1;
                        }
                    }
                    "0" => {
                        // 本词是 value 的变形（am→be）：原型行可能尚未入库，延迟回填。
                        pending_lemmas.push((
                            key.clone(),
                            value,
                            lemma_note.unwrap_or("form").to_string(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        // 中文反查：只为频率信号条目建（基础汉英边界）
        if has_signal {
            let mut terms: Vec<String> = Vec::new();
            extract_zh_terms(translation, &mut terms);
            for term in terms {
                if insert_zh
                    .execute(rusqlite::params![term, entry_id])
                    .map_err(|e| e.to_string())?
                    > 0
                {
                    stats.zh_terms += 1;
                }
            }
        }

        txn_rows += 1;
        if txn_rows >= 20_000 {
            conn.execute_batch("COMMIT; BEGIN").map_err(|e| e.to_string())?;
            txn_rows = 0;
        }
    }
    conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;

    // lemma 回填：本词 → 原型条目（kind 用本行 "1:" 的变换说明）
    for (form, base, kind) in &pending_lemmas {
        let Ok(target) = conn.query_row("SELECT id FROM entries WHERE word = ?1", [base], |r| {
            r.get::<_, i64>(0)
        }) else {
            continue; // 原型被剖面裁掉或源缺失——如实跳过
        };
        if insert_form
            .execute(rusqlite::params![form, target, kind])
            .map_err(|e| e.to_string())?
            > 0
        {
            stats.forms += 1;
        }
    }

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_entries_freq ON entries(freq, word);
         CREATE INDEX IF NOT EXISTS idx_forms_kind ON forms(entry_id);
         ANALYZE;",
    )
    .map_err(|e| format!("建索引失败: {e}"))?;
    Ok(())
}

/// basic 剖面裁剪：单词全保留（技术词不受语料频率限制）；短语需信号，或
/// 原文**全小写**且 ≤4 个纯字母 token（常见习语形态）。原文含大写字母的
/// 专名/标题短语（"A and B agglutinogens" 等）裁掉。
fn keep_entry(profile: Profile, word: &str, is_phrase: bool, has_signal: bool) -> bool {
    match profile {
        Profile::Full => true,
        Profile::Basic => {
            if !is_phrase {
                return true;
            }
            if has_signal {
                return true;
            }
            let lw = word.trim().to_lowercase();
            if lw.chars().count() > 24
                || word.chars().any(|c| c.is_ascii_uppercase())
            {
                return false;
            }
            let tokens: Vec<&str> = lw.split_whitespace().collect();
            tokens.len() <= 4
                && tokens.iter().all(|t| {
                    let mut chars = t.chars();
                    chars.next().is_some_and(|c| c.is_ascii_lowercase())
                        && chars.all(|c| c.is_ascii_lowercase() || c == '-' || c == '\'')
                })
        }
    }
}

fn exchange_field<'a>(exchange: &'a str, key: &str) -> Option<&'a str> {
    exchange.split('/').find_map(|seg| {
        let (k, v) = seg.split_once(':')?;
        (k == key).then_some(v.trim())
    })
}

/// 中文反查键抽取：字面 `\n` 分行（≤4 行，含领域行如"[计] 线程"）→ 去词性/
/// 领域前缀 → 逗分号分节（每行 ≤2 键）→ 1..=8 个纯 CJK 字符为合法键。
/// 形容词"的"尾同时登记去"的"版本（"美丽的"→"美丽"），两种反查词形都能命中。
fn extract_zh_terms(translation: &str, out: &mut Vec<String>) {
    const POS_PREFIXES: &[&str] = &[
        "n.", "v.", "vi.", "vt.", "a.", "adv.", "adj.", "prep.", "conj.", "pron.",
        "num.", "int.", "aux.", "abbr.", "suf.", "pref.", "comb.", "art.",
    ];
    let push_term = |out: &mut Vec<String>, piece: &str| -> bool {
        let len = piece.chars().count();
        if (1..=8).contains(&len)
            && piece.chars().all(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
        {
            out.push(piece.to_string());
            if let Some(stripped) = piece.strip_suffix('的')
                && !stripped.is_empty()
            {
                out.push(stripped.to_string());
            }
            true
        } else {
            false
        }
    };
    for line in translation.split("\\n").take(4) {
        let mut line = line.trim();
        // 领域前缀：[医] [计] [植] 等
        if line.starts_with('[')
            && let Some(end) = line.find(']')
        {
            line = line[end + 1..].trim_start();
        }
        // 词性前缀（可叠加：a. / vt. 等）
        loop {
            let stripped = POS_PREFIXES
                .iter()
                .find_map(|p| line.strip_prefix(p).map(|rest| (p, rest)));
            match stripped {
                Some((_, rest)) if rest.starts_with(' ') || rest.starts_with('　') => {
                    line = rest.trim_start();
                }
                _ => break,
            }
        }
        let mut pieces = 0;
        for piece in line.split([',', '，', '；', ';', '、']) {
            if pieces >= 2 {
                break;
            }
            if push_term(out, piece.trim()) {
                pieces += 1;
            }
        }
    }
}

/// DB 行流规范序内容摘要（entries → forms → zh_index 全序流）。
fn compute_basic_digest(conn: &Connection) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let mut feed = |sql: &str| -> Result<(), String> {
        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
        let col_count = stmt.column_count();
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            for i in 0..col_count {
                let value: rusqlite::types::Value =
                    row.get(i).map_err(|e| e.to_string())?;
                match value {
                    rusqlite::types::Value::Null => hasher.update(&[0]),
                    rusqlite::types::Value::Integer(n) => {
                        hasher.update(n.to_le_bytes());
                    }
                    rusqlite::types::Value::Real(f) => {
                        hasher.update(f.to_le_bytes());
                    }
                    rusqlite::types::Value::Text(t) => {
                        hasher.update(t.as_bytes());
                    }
                    rusqlite::types::Value::Blob(b) => {
                        hasher.update(&b);
                    }
                }
                hasher.update(&[0x1f]);
            }
            hasher.update(&[0x1e]);
        }
        Ok(())
    };
    let _ = feed("SELECT id, word, display, phonetic, definition, translation, pos, tag, collins, oxford, freq, is_phrase FROM entries ORDER BY id");
    let _ = feed("SELECT form, entry_id, kind FROM forms ORDER BY form, entry_id");
    let _ = feed("SELECT term, entry_id FROM zh_index ORDER BY term, entry_id");
    format!("{:x}", hasher.finalize())
}

fn compute_ety_digest(conn: &Connection) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let Ok(mut stmt) = conn.prepare("SELECT word, roots FROM ety ORDER BY word") else {
        return String::new();
    };
    if let Ok(mut rows) = stmt.query([]) {
        while let Ok(Some(row)) = rows.next() {
            let word: String = row.get(0).unwrap_or_default();
            let roots: String = row.get(1).unwrap_or_default();
            hasher.update(word.as_bytes());
            hasher.update(&[0x1f]);
            hasher.update(roots.as_bytes());
            hasher.update(&[0x1e]);
        }
    }
    format!("{:x}", hasher.finalize())
}

fn write_meta(
    conn: &Connection,
    dataset_id: &str,
    content_sha256: &str,
    stats: &BuildStats,
) -> Result<(), String> {
    let (name, url, license) = ECDICT_SOURCE;
    conn.execute_batch(&format!(
        "INSERT OR REPLACE INTO meta VALUES('format_version','{FORMAT_VERSION}');
         INSERT OR REPLACE INTO meta VALUES('dataset_id','{dataset_id}');
         INSERT OR REPLACE INTO meta VALUES('source_name','{name}');
         INSERT OR REPLACE INTO meta VALUES('source_url','{url}');
         INSERT OR REPLACE INTO meta VALUES('source_license','{license}');
         INSERT OR REPLACE INTO meta VALUES('built_at','{}');
         INSERT OR REPLACE INTO meta VALUES('content_sha256','{content_sha256}');
         INSERT OR REPLACE INTO meta VALUES('entries','{}');
         INSERT OR REPLACE INTO meta VALUES('phrases','{}');",
        now_iso(),
        stats.entries,
        stats.phrases,
    ))
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn write_dataset_json(
    package_dir: &Path,
    files: &[(&str, &Path)],
    dataset_id: &str,
    content_sha256: &str,
    opts: &BuildOptions,
    counts: &[(&str, u64)],
) -> Result<(), String> {
    let (name, url, license) = ECDICT_SOURCE;
    let mut files_json = serde_json::Map::new();
    for (logical, path) in files {
        let sha = sha256_file(path)?;
        let bytes = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
        files_json.insert(
            logical.to_string(),
            serde_json::json!({"sha256": sha, "bytes": bytes}),
        );
    }
    let counts_json: serde_json::Map<String, serde_json::Value> = counts
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
        .collect();
    let manifest = serde_json::json!({
        "format_version": FORMAT_VERSION,
        "dataset_id": dataset_id,
        "plugin_id": PLUGIN_ID,
        "version": opts.version,
        "profile": if dataset_id == ETY_DATASET_ID { "etymology" } else { opts.profile.wire_name() },
        "name": opts.name,
        "source": {
            "name": name,
            "url": url,
            "license": license,
            "revision": opts.source_revision.clone().unwrap_or_default(),
        },
        "files": files_json,
        "content_sha256": content_sha256,
        "counts": counts_json,
        "built_at": now_iso(),
    });
    std::fs::write(
        package_dir.join("dataset.json"),
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写 dataset.json 失败: {e}"))?;
    Ok(())
}

fn open_readonly(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("打开 {} 失败: {e}", path.display()))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn now_iso() -> String {
    // 无 chrono 依赖：用系统秒手拼 UTC ISO-8601（日期精度足够溯源）。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 公历日期换算（ civile_from_days, Howard Hinnant 算法）
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T00:00:00Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 测试夹具：程序化写 ECDICT 形态的 13 列 CSV（csv writer 保证引号/列数正确）。
#[cfg(test)]
pub(crate) fn write_csv_fixture(path: &Path, rows: &[&[&str]]) {
    let mut w = csv::Writer::from_path(path).expect("创建 fixture csv");
    w.write_record([
        "word", "phonetic", "definition", "translation", "pos", "collins", "oxford",
        "tag", "bnc", "frq", "exchange", "detail", "audio",
    ])
    .unwrap();
    for row in rows {
        assert_eq!(row.len(), 13, "fixture 行必须 13 列: {row:?}");
        w.write_record(*row).unwrap();
    }
    w.flush().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    fn fixture_csv(dir: &Path) -> PathBuf {
        // 覆盖：单词/短语/信号/无信号/词形/中文反查/多义项
        let csv = dir.join("fixture.csv");
        write_csv_fixture(
            &csv,
            &[
                &["apple", "ˈæpl", "n. fruit", "n. 苹果, 家伙\\n[医] 苹果", "n:100", "0", "1", "zk gk", "1000", "900", "s:apples", "", ""],
                &["go", "ɡəʊ", "", "vi. 去", "vi:100", "5", "1", "zk", "100", "90", "p:went/i:going/d:gone/3:goes/s:goes", "", ""],
                &["went", "went", "", "go 的过去式", "", "", "", "", "0", "0", "0:go/1:p", "", ""],
                &["black tea", "", "", "n. 红茶（字面黑茶）", "n:100", "0", "0", "", "0", "0", "", "", ""],
                &["A and B agglutinogens", "", "", "[医] AB 凝集原", "", "0", "0", "", "0", "0", "", "", ""],
                &["compiler", "kəmˈpaɪlə", "", "n. 编辑者\\n[计] 编译程序", "n:100", "0", "0", "", "9000", "20000", "s:compilers", "", ""],
                &["async", "", "", "异步的", "", "0", "0", "", "", "", "", "", ""],
            ],
        );
        csv
    }

    #[test]
    fn build_basic_and_query_indexes() {
        let tmp = tempdir();
        let csv = fixture_csv(&tmp);
        let out = build_basic_dataset(
            &csv,
            &BuildOptions::default(),
            &tmp.join("out"),
        )
        .unwrap();

        // 剖面：单词全留（含 async 无信号）；无信号杂短语裁掉；简单短语保留
        assert!(out.stats.entries >= 5, "entries={}", out.stats.entries);
        let conn = open_readonly(&out.package_dir.join("dict.db")).unwrap();
        let count = |word: &str| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM entries WHERE word=?1", [word], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count("apple"), 1);
        assert_eq!(count("async"), 1, "无信号单词应保留（技术词）");
        assert_eq!(count("black tea"), 1, "简单小写短语应保留");
        assert_eq!(count("a and b agglutinogens"), 0, "大写无信号短语应裁掉");

        // 词形：s:apples（apple 行）+ 0:go（went 行）
        let base_of = |form: &str| -> Option<String> {
            conn.query_row(
                "SELECT e.word, f.kind FROM forms f JOIN entries e ON e.id=f.entry_id WHERE f.form=?1",
                [form],
                |r| r.get::<_, String>(0).map(|w| format!("{w}:{}", r.get::<_, String>(1).unwrap_or_default())),
            )
            .ok()
        };
        assert_eq!(base_of("apples").as_deref(), Some("apple:s"));
        assert_eq!(base_of("went").as_deref(), Some("go:p"), "went→go（0:go/1:p）");
        assert_eq!(base_of("going").as_deref(), Some("go:i"));

        // 中文反查：apple（oxford=1 有信号）登记；black tea 无信号不登记
        let zh_hit = |term: &str| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM zh_index WHERE term=?1", [term], |r| r.get(0))
                .unwrap()
        };
        assert!(zh_hit("苹果") >= 1);
        assert_eq!(zh_hit("家伙") >= 1, true);
        assert_eq!(zh_hit("红茶"), 0, "无信号短语不建反查键");

        // verify：摘要与文件哈希自洽
        assert!(verify_dataset(&out.package_dir).unwrap());
        // 篡改 DB → verify 失败
        std::fs::write(out.package_dir.join("dict.db"), b"broken").unwrap();
        assert!(!verify_dataset(&out.package_dir).unwrap());
    }

    #[test]
    fn quality_samples_pass_on_fixture() {
        let tmp = tempdir();
        let csv = fixture_csv(&tmp);
        let out = build_basic_dataset(&csv, &BuildOptions::default(), &tmp.join("out2")).unwrap();
        let samples = tmp.join("samples.json");
        std::fs::write(
            &samples,
            r#"{"exact":[{"word":"apple","expect_translation_contains":"苹果"}],
                "multi_sense":[{"word":"compiler","min_sense_lines":2}],
                "form":[{"form":"went","base":"go"}],
                "phrase":[{"word":"black tea"}],
                "tech":[{"word":"async"}],
                "reverse_zh":[{"term":"苹果","expect_contains_word":"apple"}]}"#,
        )
        .unwrap();
        check_quality_samples(&out.package_dir, &samples).unwrap();
        // 反例：未收录词 → 失败
        std::fs::write(
            &samples,
            r#"{"exact":[{"word":"nonexistentwordxyz"}]}"#,
        )
        .unwrap();
        assert!(check_quality_samples(&out.package_dir, &samples).is_err());
    }

    #[test]
    fn etymology_build_and_verify() {
        let tmp = tempdir();
        let wordroot = tmp.join("wordroot.json");
        std::fs::write(
            &wordroot,
            r#"{"hom":{"meaning":"man, human","class":"root","example":["homicide","hominoid"],"origin":"Latin"},
                "-less":{"meaning":"without","class":"suffix","example":["aimless","careless"],"origin":"Old English"}}"#,
        )
        .unwrap();
        let out = build_etymology_dataset(&wordroot, "1.0.0", &tmp.join("ety")).unwrap();
        let conn = open_readonly(&out.package_dir.join("ety.db")).unwrap();
        let roots: String = conn
            .query_row("SELECT roots FROM ety WHERE word='homicide'", [], |r| r.get(0))
            .unwrap();
        assert!(roots.contains("hom"));
        assert!(roots.contains("man, human"));
        assert!(verify_dataset(&out.package_dir).unwrap());
    }

    #[test]
    fn zh_term_extraction_rules() {
        let mut out = Vec::new();
        extract_zh_terms("n. 苹果, 家伙\\n[医] 苹果制品\\nvt. 使崩溃", &mut out);
        assert!(out.contains(&"苹果".to_string()));
        assert!(out.contains(&"家伙".to_string()));
        // [医] 前缀剥离后的词也可作键
        assert!(out.iter().any(|t| t.starts_with("苹果")));
        assert!(!out.iter().any(|t| t.contains('，') || t.is_empty()));
        // 每行最多 2 键
        let mut out2 = Vec::new();
        extract_zh_terms("甲, 乙, 丙, 丁", &mut out2);
        assert_eq!(out2.len(), 2);
        // 非纯 CJK（混英文/数字）不成键
        let mut out3 = Vec::new();
        extract_zh_terms("HTML5 页面", &mut out3);
        assert!(out3.is_empty());
    }
}
