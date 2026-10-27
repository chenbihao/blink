//! 词典查询服务（0.28.4 §五.3）——按 active.json 指针打开固定词库快照，
//! 提供有界的精确/词形/前缀/拼写候选与基础汉英反查。
//!
//! **设计约束**：
//! - 查询固定词库快照：`open()` 解析 active.json 后以只读句柄持有到指针变化；
//!   词库升级换版本目录（新文件），旧句柄继续读旧文件（Windows 文件语义），
//!   `reload_if_changed()` 检测指针变化后原子换连接——不原地覆盖使用中的 DB。
//! - 基础快查纯 SQL（LIKE 前缀 + 频率排序），无额外脚本解释器依赖；
//! - 拼写候选：编辑距离 ≤2 的常见词，量有界（候选池限定同首字母 + 词长邻域
//!   内词频前 LIMIT_SPELL_POOL 词，输出 ≤ 4 条）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;

/// 基础词库数据集 id（与 builder::BASIC_DATASET_ID 一致）。
pub const BASIC_DATASET_ID: &str = "dictionary-basic";
/// 词源包 id。
pub const ETY_DATASET_ID: &str = "dictionary-etymology";
/// 每类查询的输出上限（有界响应）。
pub const LIMIT_RESULTS: usize = 8;
/// 前缀候选扫描上限。
const LIMIT_PREFIX_SCAN: usize = 20;
/// 拼写候选池上限（同首字母 + 词长邻域内按词频取前 N；邻域由编辑距离 ≤2
/// 的必要条件「词长差 ≤2」圈定，保证真实拼错的中频词也能进池）。
const LIMIT_SPELL_POOL: usize = 1000;

/// 词库加载状态。
#[derive(Debug, Clone, PartialEq)]
pub enum DictState {
    /// datasets 目录未注入（宿主未握手或非 v2）。
    NoDirs,
    /// 词库未安装（无 active.json）。
    NotInstalled,
    /// 词库损坏（指针无效/DB 打不开/结构不符）——如实报错，重装修复。
    Corrupt(String),
    /// 就绪。
    Ready,
}

/// 词条查询结果（lookup tool 的 data / 页面详情共用）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WordEntry {
    pub word: String,
    pub display: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub phonetic: String,
    /// 英文义项（字面 `\n` 分行，构建期保真）。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub definition: String,
    /// 中文释义（字面 `\n` 分行）。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub translation: String,
    /// 词性占比（如 "n:46/v:54"）。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pos: String,
    /// 考试标签（zk/gk/cet4...）。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tag: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub collins: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub oxford: i64,
    /// 语料频率排名（越小越常用；0 = 无信号）——AI/排序参考。
    #[serde(skip_serializing_if = "is_zero")]
    pub freq: i64,
    /// 本词是别的词的变形时的还原提示（如 "went → go（过去式）"）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lemma_of: Option<String>,
    /// 词形衍生表（本词的变形 → 变换种类）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub forms: Vec<FormEntry>,
    /// 词源（词源包已装时填充；缺失如实留空——不 AI 兜底）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub etymology: Vec<EtymologyEntry>,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// 词形衍生条目。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FormEntry {
    pub form: String,
    /// ECDICT exchange 种类（p/d/i/3/r/t/s）或 lemma 说明。
    pub kind: String,
}

/// 词源条目（词根/词缀信息）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EtymologyEntry {
    pub root: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub meaning: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub class: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub origin: String,
}

/// 汉英反查条目。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ZhReverseEntry {
    pub word: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub translation: String,
    pub freq: i64,
}

/// 词库服务。`Mutex` 保护快照换装；查询本身只读共享。
pub struct DictionaryService {
    datasets_dir: Mutex<Option<PathBuf>>,
    /// 基础词库快照（连接 + active 版本；指针变化触发换装）。
    basic: Mutex<Option<LoadedDb>>,
    /// 词源包已验证的 active 版本（词源查询每次开只读连接，本地 open 开销
    /// 微秒级；这里只缓存"结构验证过"的版本号，避免重复校验）。
    ety_version: Mutex<Option<String>>,
}

struct LoadedDb {
    /// active.json 指向的版本（快照标识；指针变化触发换装）。
    version: String,
    conn: Connection,
}

impl DictionaryService {
    pub fn new() -> Self {
        DictionaryService {
            datasets_dir: Mutex::new(None),
            basic: Mutex::new(None),
            ety_version: Mutex::new(None),
        }
    }

    /// v2 握手注入 datasets 目录（initialize 帧）。
    pub fn set_datasets_dir(&self, dir: PathBuf) {
        *self.datasets_dir.lock().unwrap() = Some(dir);
        // 换目录即失效既有快照
        *self.basic.lock().unwrap() = None;
        *self.ety_version.lock().unwrap() = None;
    }

    fn datasets_base(&self) -> Option<PathBuf> {
        self.datasets_dir.lock().unwrap().clone()
    }

    /// 词库状态（readiness 对齐的进程内视角；工具返回的状态字段用它）。
    pub fn state(&self) -> DictState {
        let Some(base) = self.datasets_base() else {
            return DictState::NoDirs;
        };
        match resolve_active(&base.join(BASIC_DATASET_ID)) {
            Active::NotMounted => DictState::NotInstalled,
            Active::Corrupt => DictState::Corrupt("active.json 指针无效".into()),
            Active::Mounted(version) => {
                // 尝试打开验证结构
                match self.ensure_basic(&base, &version) {
                    Ok(()) => DictState::Ready,
                    Err(e) => DictState::Corrupt(e),
                }
            }
        }
    }

    /// 打开（或复用）基础词库快照；指针版本变化时换装。
    fn ensure_basic(&self, base: &Path, version: &str) -> Result<(), String> {
        let mut guard = self.basic.lock().unwrap();
        if let Some(loaded) = guard.as_ref()
            && loaded.version == version
        {
            return Ok(());
        }
        let db_path = base.join(BASIC_DATASET_ID).join(version).join("dict.db");
        let conn = open_readonly(&db_path)?;
        verify_basic_shape(&conn)?;
        *guard = Some(LoadedDb {
            version: version.to_string(),
            conn,
        });
        Ok(())
    }

    /// 打开（或复用）词源包快照；未装返回 Ok(None)。首次解析时校验表结构，
    /// 版本号缓存后跳过重复校验（词源查询每次新开只读连接，开销微秒级）。
    fn ety_conn(&self) -> Result<Option<Connection>, String> {
        let Some(base) = self.datasets_base() else {
            return Ok(None);
        };
        let version = match resolve_active(&base.join(ETY_DATASET_ID)) {
            Active::Mounted(v) => v,
            _ => {
                *self.ety_version.lock().unwrap() = None;
                return Ok(None);
            }
        };
        let db_path = base.join(ETY_DATASET_ID).join(&version).join("ety.db");
        {
            let mut cached = self.ety_version.lock().unwrap();
            if cached.as_deref() != Some(&version) {
                let conn = open_readonly(&db_path)?;
                let has_table = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ety'",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .map_err(|e| e.to_string())?;
                if has_table == 0 {
                    return Err("词源包结构不符（缺少 ety 表）".into());
                }
                *cached = Some(version);
            }
        }
        Ok(Some(open_readonly(&db_path)?))
    }

    /// 查词全路径：精确词头 → 词形还原 → 短语精确 → 前缀候选 → 拼写候选。
    /// 返回 (命中详情, 候选列表)。未命中时 candidates 可能为空（转翻译入口
    /// 由调用方决定）。
    pub fn lookup(&self, query: &str) -> Result<(Option<WordEntry>, Vec<Candidate>), String> {
        let base = self
            .datasets_base()
            .ok_or_else(|| "datasets 目录未注入（宿主未完成 v2 握手）".to_string())?;
        let version = match resolve_active(&base.join(BASIC_DATASET_ID)) {
            Active::Mounted(v) => v,
            Active::NotMounted => return Err("词库未安装".into()),
            Active::Corrupt => return Err("词库损坏（active.json 指针无效），请重装".into()),
        };
        self.ensure_basic(&base, &version)?;
        let guard = self.basic.lock().unwrap();
        let conn = &guard.as_ref().expect("ensure 后必存在").conn;

        let q = query.trim();
        if q.is_empty() {
            return Ok((None, Vec::new()));
        }
        let key = q.to_lowercase();

        // 1. 精确词头（含短语）。词头命中但同时也是别的词的变形（如 went）
        //    时补 lemma 还原提示。
        if let Some(mut entry) = fetch_entry(conn, &key)? {
            entry = self.hydrate_entry(conn, entry)?;
            if entry.lemma_of.is_none()
                && let Some((base, kind)) = fetch_lemma_hint(conn, &key)?
            {
                entry.lemma_of =
                    Some(format!("{} → {base}（{}）", entry.display, form_display(&kind)));
            }
            return Ok((Some(entry), Vec::new()));
        }

        // 2. 词形还原：exact form → 原型条目
        if let Some(entry) = fetch_entry_by_form(conn, &key)? {
            return Ok((Some(self.hydrate_entry(conn, entry)?), Vec::new()));
        }

        // 3. 候选：拼写建议优先（短输入大概率是拼错，如 aple→apple），
        //    前缀匹配补足（appl→application 等浏览意图）。两者合并去重。
        let spell = spell_candidates(conn, &key)?;
        let prefix = prefix_candidates(conn, &key)?;
        let mut candidates = spell;
        for c in prefix {
            if !candidates.iter().any(|x| x.word == c.word) {
                candidates.push(c);
            }
        }
        candidates.truncate(LIMIT_RESULTS);
        Ok((None, candidates))
    }

    /// 词条增强：词形衍生 + 词源（词源包装了才有）。
    fn hydrate_entry(&self, conn: &Connection, mut entry: WordEntry) -> Result<WordEntry, String> {
        entry.forms = fetch_forms(conn, &entry.word)?;
        if let Ok(Some(ety_conn)) = self.ety_conn() {
            entry.etymology = fetch_etymology(&ety_conn, &entry.word)?;
        }
        Ok(entry)
    }

    /// 基础汉英反查（中文 → 英文候选，频率排序）。
    pub fn reverse_zh(&self, term: &str) -> Result<Vec<ZhReverseEntry>, String> {
        let base = self
            .datasets_base()
            .ok_or_else(|| "datasets 目录未注入".to_string())?;
        let version = match resolve_active(&base.join(BASIC_DATASET_ID)) {
            Active::Mounted(v) => v,
            Active::NotMounted => return Err("词库未安装".into()),
            Active::Corrupt => return Err("词库损坏，请重装".into()),
        };
        self.ensure_basic(&base, &version)?;
        let guard = self.basic.lock().unwrap();
        let conn = &guard.as_ref().expect("ensure 后必存在").conn;

        let term = term.trim();
        if term.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = conn
            .prepare(
                "SELECT e.word, e.translation, e.freq FROM zh_index z
                 JOIN entries e ON e.id = z.entry_id
                 WHERE z.term = ?1 ORDER BY e.freq LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![term, LIMIT_RESULTS as i64], |r| {
                Ok(ZhReverseEntry {
                    word: r.get(0)?,
                    translation: r.get(1)?,
                    freq: r.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 主窗快查摘要（第一条中文释义行）。
    pub fn summarize(&self, entry: &WordEntry) -> String {
        entry
            .translation
            .split("\\n")
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(&entry.translation)
            .to_string()
    }
}

/// 前缀/拼写候选。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Candidate {
    pub word: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub translation: String,
    /// 候选类型：prefix（前缀）/ spell（拼写建议）。
    pub kind: &'static str,
    pub freq: i64,
}

enum Active {
    NotMounted,
    Corrupt,
    Mounted(String),
}

/// active.json 解析（与宿主 domain/plugin/dataset.rs 同语义的进程内副本）。
fn resolve_active(dataset_dir: &Path) -> Active {
    if !dataset_dir.is_dir() {
        return Active::NotMounted;
    }
    let Ok(raw) = std::fs::read_to_string(dataset_dir.join("active.json")) else {
        return Active::NotMounted;
    };
    #[derive(serde::Deserialize)]
    struct Ptr {
        version: String,
    }
    let Ok(ptr) = serde_json::from_str::<Ptr>(&raw) else {
        return Active::Corrupt;
    };
    if ptr.version.is_empty()
        || !dataset_dir.join(&ptr.version).join("dataset.json").is_file()
    {
        return Active::Corrupt;
    }
    Active::Mounted(ptr.version)
}

fn open_readonly(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("打开 {} 失败: {e}", path.display()))
}

fn verify_basic_shape(conn: &Connection) -> Result<(), String> {
    let ok = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('entries','forms','zh_index')",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| e.to_string())?;
    if ok < 3 {
        return Err("词库结构不符（缺少 entries/forms/zh_index 表）".into());
    }
    Ok(())
}

const ENTRY_COLS: &str =
    "id, word, display, phonetic, definition, translation, pos, tag, collins, oxford, freq";

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<(i64, WordEntry)> {
    Ok((
        row.get(0)?,
        WordEntry {
            word: row.get(1)?,
            display: row.get(2)?,
            phonetic: row.get(3)?,
            definition: row.get(4)?,
            translation: row.get(5)?,
            pos: row.get(6)?,
            tag: row.get(7)?,
            collins: row.get(8)?,
            oxford: row.get(9)?,
            freq: row.get(10)?,
            lemma_of: None,
            forms: Vec::new(),
            etymology: Vec::new(),
        },
    ))
}

fn fetch_entry(conn: &Connection, key: &str) -> Result<Option<WordEntry>, String> {
    let sql = format!("SELECT {ENTRY_COLS} FROM entries WHERE word = ?1");
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    match stmt.query_row([key], |r| row_to_entry(r)) {
        Ok((_, entry)) => Ok(Some(entry)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// 词头命中时的 lemma 还原提示（该词同时是别的词的变形）：返回 (原型, 变换种类)。
fn fetch_lemma_hint(conn: &Connection, word: &str) -> Result<Option<(String, String)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT e.word, f.kind FROM forms f
             JOIN entries e ON e.id = f.entry_id
             WHERE f.form = ?1 AND e.word != ?1 LIMIT 1",
        )
        .map_err(|e| e.to_string())?;
    match stmt.query_row([word], |r| Ok((r.get(0)?, r.get(1)?))) {
        Ok(pair) => Ok(Some(pair)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn fetch_entry_by_form(conn: &Connection, form: &str) -> Result<Option<WordEntry>, String> {    let sql = format!(
        "SELECT {ENTRY_COLS}, f.kind FROM entries e
         JOIN forms f ON f.entry_id = e.id
         WHERE f.form = ?1 LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    match stmt.query_row([form], |r| {
        let (id, mut entry) = row_to_entry(r)?;
        let kind: String = r.get(11)?;
        entry.lemma_of = Some(format!("{} → {}（{kind}）", entry.display, form_display(&kind)));
        Ok((id, entry))
    }) {
        Ok((_, entry)) => Ok(Some(entry)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// ECDICT exchange 种类 → 中文说明。
fn form_display(kind: &str) -> &'static str {
    match kind {
        "p" => "过去式",
        "d" => "过去分词",
        "i" => "现在分词",
        "3" => "第三人称单数",
        "r" => "比较级",
        "t" => "最高级",
        "s" => "名词复数",
        _ => "词形",
    }
}

fn fetch_forms(conn: &Connection, word: &str) -> Result<Vec<FormEntry>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT f.form, f.kind FROM forms f JOIN entries e ON e.id = f.entry_id
             WHERE e.word = ?1 ORDER BY f.kind, f.form",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([word], |r| {
            Ok(FormEntry {
                form: r.get(0)?,
                kind: r.get(1)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn fetch_etymology(conn: &Connection, word: &str) -> Result<Vec<EtymologyEntry>, String> {
    let mut stmt = conn
        .prepare("SELECT roots FROM ety WHERE word = ?1")
        .map_err(|e| e.to_string())?;
    let roots: Option<String> = match stmt.query_row([word], |r| r.get(0)) {
        Ok(v) => Some(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.to_string()),
    };
    let Some(roots) = roots else {
        return Ok(Vec::new());
    };
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&roots).unwrap_or_default();
    Ok(parsed
        .into_iter()
        .map(|v| EtymologyEntry {
            root: v["root"].as_str().unwrap_or_default().to_string(),
            meaning: v["meaning"].as_str().unwrap_or_default().to_string(),
            class: v["class"].as_str().unwrap_or_default().to_string(),
            origin: v["origin"].as_str().unwrap_or_default().to_string(),
        })
        .collect())
}

fn prefix_candidates(conn: &Connection, key: &str) -> Result<Vec<Candidate>, String> {
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '\'') {
        return Ok(Vec::new()); // 含中文等 → 非英文前缀场景
    }
    let mut stmt = conn
        .prepare(
            "SELECT word, translation, freq FROM entries
             WHERE word LIKE ?1 ESCAPE '\\' AND word != ?2
             ORDER BY freq, word LIMIT ?3",
        )
        .map_err(|e| e.to_string())?;
    let pattern = format!(
        "{}%",
        key.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
    );
    let rows = stmt
        .query_map(
            rusqlite::params![pattern, key, LIMIT_PREFIX_SCAN as i64],
            |r| {
                Ok(Candidate {
                    word: r.get(0)?,
                    translation: r.get(1)?,
                    kind: "prefix",
                    freq: r.get(2)?,
                })
            },
        )
        .map_err(|e| e.to_string())?;
    let mut out: Vec<Candidate> = rows.filter_map(|r| r.ok()).collect();
    out.truncate(LIMIT_RESULTS);
    Ok(out)
}

fn spell_candidates(conn: &Connection, key: &str) -> Result<Vec<Candidate>, String> {
    let word = key.trim();
    let word_len = word.chars().count();
    if !(3..=24).contains(&word_len) || !word.chars().all(|c| c.is_ascii_lowercase()) {
        return Ok(Vec::new());
    }
    // 高频池：同首字母 + 词长邻域（编辑距离 ≤2 ⇒ 词长差 ≤2），按词频取前
    // LIMIT_SPELL_POOL。全库前 60 超高频词池召回不了中频词的真实拼错
    // （helo→hello、monogmay→monogamy），邻域池在索引面（idx_entries_word
    // 前缀范围 + idx_entries_freq 排序）上仍是几千行量级的有界扫描。
    let pattern = format!("{}%", word.chars().next().expect("门槛已保证非空"));
    let mut stmt = conn
        .prepare(
            "SELECT word, translation, freq FROM entries
             WHERE word LIKE ?1 ESCAPE '\\' AND length(word) BETWEEN ?2 AND ?3
             ORDER BY freq LIMIT ?4",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            rusqlite::params![
                pattern,
                word_len.saturating_sub(2) as i64,
                (word_len + 2) as i64,
                LIMIT_SPELL_POOL as i64
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?;
    let mut hits: Vec<(usize, i64, String, String)> = Vec::new();
    for row in rows.filter_map(|r| r.ok()) {
        if let Some(dist) = edit_distance_at_most(word, &row.0, 2) {
            hits.push((dist, row.2, row.0, row.1));
        }
    }
    // 距离优先：dist 1 的 apple 比 dist 2 的 apply 更可能是 aple 的目标词——
    // 纯词频序会在凑满 4 条时让更常用的 dist 2 词挤掉 dist 1 词。同距离内
    // 再按词频（更常用优先）。池 ≤ LIMIT_SPELL_POOL，全量收集后排序代价可忽略。
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(hits
        .into_iter()
        .take(4)
        .map(|(_dist, freq, word, translation)| Candidate {
            word,
            translation,
            kind: "spell",
            freq,
        })
        .collect())
}

/// 限定上限的编辑距离（early-exit；候选词长度差 > max 直接 None）。
/// 命中返回 Some(实际距离)，供调用方按距离分级排序。
fn edit_distance_at_most(a: &str, b: &str, max: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        let mut row_min = curr[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
            row_min = row_min.min(curr[j + 1]);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    (prev[b.len()] <= max).then_some(prev[b.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{build_basic_dataset, build_etymology_dataset, BuildOptions};

    /// 构建一个 fixture 词库并挂载到 datasets 目录，返回服务。
    fn service_with_fixture() -> (DictionaryService, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.keep();
        let csv = tmp.join("fixture.csv");
        crate::builder::write_csv_fixture(
            &csv,
            &[
                &["apple", "ˈæpl", "n. fruit", "n. 苹果, 家伙\\n[医] 苹果", "n:100", "0", "1", "zk gk", "1000", "900", "s:apples", "", ""],
                &["go", "ɡəʊ", "", "vi. 去", "vi:100", "5", "1", "zk", "100", "90", "p:went/i:going/d:gone/3:goes/s:goes", "", ""],
                &["went", "went", "", "go 的过去式", "", "", "", "", "0", "0", "0:go/1:p", "", ""],
                &["application", "ˌæplɪˈkeɪʃn", "n. request", "n. 申请, 应用", "n:100", "3", "0", "cet4", "500", "400", "", "", ""],
                &["monogamy", "məˈnɒɡəmi", "n. one spouse", "n. 一夫一妻制", "n:100", "0", "0", "gre", "700", "600", "", "", ""],
                &["axe", "æks", "n. tool", "n. 斧\\nv. 削减", "n:100", "0", "0", "zk gk", "100", "90", "", "", ""],
            ],
        );
        let out = build_basic_dataset(&csv, &BuildOptions::default(), &tmp.join("out")).unwrap();

        // 挂载：datasets/dictionary-basic/<version>/ + active.json
        let base = tmp.join("datasets").join(BASIC_DATASET_ID);
        let version = "1.0.0";
        std::fs::create_dir_all(base.join(version)).unwrap();
        // 复制产物（包目录结构 = 版本目录）
        for entry in std::fs::read_dir(&out.package_dir).unwrap().flatten() {
            let to = base.join(version).join(entry.file_name());
            std::fs::copy(entry.path(), &to).unwrap();
        }
        std::fs::write(
            base.join("active.json"),
            format!("{{\"version\":\"{version}\"}}"),
        )
        .unwrap();

        let svc = DictionaryService::new();
        svc.set_datasets_dir(tmp.join("datasets"));
        (svc, tmp)
    }

    #[test]
    fn lookup_exact_form_and_prefix() {
        let (svc, _tmp) = service_with_fixture();

        // 精确
        let (entry, cands) = svc.lookup("Apple").unwrap();
        assert!(cands.is_empty());
        let entry = entry.expect("apple 应命中");
        assert!(entry.translation.contains("苹果"));
        assert_eq!(entry.forms.len(), 1);
        assert_eq!(entry.forms[0].form, "apples");

        // 词形还原：went 是独立词头（exact 命中并带 lemma 提示，见上）；
        // 非 词头 的变形（如 going）在这里还原到原型条目。
        let (entry, _) = svc.lookup("going").unwrap();
        let entry = entry.expect("going 应还原到 go");
        assert_eq!(entry.word, "go");
        assert!(entry.lemma_of.as_deref().unwrap().contains("现在分词"));

        // 词头命中 + lemma 提示：went 自身是词条，同时提示原型 go
        let (entry, _) = svc.lookup("went").unwrap();
        let entry = entry.expect("went 应命中自身词条");
        assert_eq!(entry.word, "went");
        assert!(
            entry.lemma_of.as_deref().unwrap().contains("go"),
            "went 应带 go 的还原提示: {:?}",
            entry.lemma_of
        );

        // 候选：appl 前缀查询——apple 以拼写建议优先出现（appl→apple 编辑距离 1）
        let (entry, cands) = svc.lookup("appl").unwrap();
        assert!(entry.is_none(), "appl 不是词头");
        assert!(
            cands.iter().any(|c| c.word == "apple"),
            "appl 应给 apple 候选: {cands:?}"
        );
        assert!(
            cands.iter().any(|c| c.kind == "prefix" && c.word == "application"),
            "appl 也应有前缀候选 application: {cands:?}"
        );
        assert_eq!(
            cands[0].kind, "spell",
            "拼写建议应排在最前（拼错意图优先）: {cands:?}"
        );

        // 拼写候选：aple → apple（编辑距离 1，拼写优先于前缀冷僻词）
        let (_, cands) = svc.lookup("aple").unwrap();
        assert!(
            cands.iter().any(|c| c.word == "apple" && c.kind == "spell"),
            "aple 应给出 apple 拼写建议: {cands:?}"
        );
        assert_eq!(cands[0].word, "apple", "apple 应为首个候选: {cands:?}");

        // 未收录
        let (entry, cands) = svc.lookup("zzzzzzz").unwrap();
        assert!(entry.is_none() && cands.is_empty());
    }

    #[test]
    fn spell_pool_letter_and_length_window() {
        let (svc, _tmp) = service_with_fixture();

        // 中频词拼写错误可召回：monogmay → monogamy（编辑距离 2，同首字母
        // 长度段内）——旧的全库前 60 高频池召回不了这种词
        let (_, cands) = svc.lookup("monogmay").unwrap();
        assert!(
            cands.iter().any(|c| c.word == "monogamy" && c.kind == "spell"),
            "monogmay 应给出 monogamy 拼写建议: {cands:?}"
        );

        // 长度窗排除：aple（词长 4，窗 2-6）的拼写池不含 11 字母的
        // application；前缀候选也不匹配（aple ≠ appl 前缀）
        let (_, cands) = svc.lookup("aple").unwrap();
        assert!(
            !cands.iter().any(|c| c.word == "application"),
            "aple 不应给 application 候选: {cands:?}"
        );

        // 距离优先于词频：aple 的候选中 dist 1 的 apple（freq 900）排在
        // dist 2 但更高频的 axe（freq 90）之前
        let (_, cands) = svc.lookup("aple").unwrap();
        let i_apple = cands.iter().position(|c| c.word == "apple").unwrap();
        let i_axe = cands.iter().position(|c| c.word == "axe").unwrap();
        assert!(
            i_apple < i_axe,
            "距离 1 的 apple 应排在距离 2 的 axe 之前: {cands:?}"
        );
    }

    #[test]
    fn reverse_zh_and_states() {
        let (svc, tmp) = service_with_fixture();

        // 状态：Ready
        assert_eq!(svc.state(), DictState::Ready);

        // 汉英反查：苹果 → apple
        let hits = svc.reverse_zh("苹果").unwrap();
        assert!(hits.iter().any(|h| h.word == "apple"));

        // 未安装：无 datasets 目录
        let svc2 = DictionaryService::new();
        assert_eq!(svc2.state(), DictState::NoDirs);
        svc2.set_datasets_dir(tmp.join("nowhere"));
        assert_eq!(svc2.state(), DictState::NotInstalled);

        // 损坏：active.json 指向不存在版本
        let corrupt_root = tmp.join("corrupt-datasets");
        std::fs::create_dir_all(corrupt_root.join(BASIC_DATASET_ID)).unwrap();
        std::fs::write(
            corrupt_root.join(BASIC_DATASET_ID).join("active.json"),
            "{\"version\":\"9.9.9\"}",
        )
        .unwrap();
        let svc3 = DictionaryService::new();
        svc3.set_datasets_dir(corrupt_root);
        assert!(matches!(svc3.state(), DictState::Corrupt(_)));
        let err = svc3.lookup("apple").unwrap_err();
        assert!(err.contains("损坏"));
    }

    #[test]
    fn etymology_enriches_entry_when_installed() {
        let (svc, tmp) = service_with_fixture();

        // 未装词源包：etymology 为空（如实降级，不伪造）
        let (entry, _) = svc.lookup("monogamy").unwrap();
        assert!(entry.unwrap().etymology.is_empty());

        // 装词源包（gam → monogamy 示例词）
        let wordroot = tmp.join("wordroot.json");
        std::fs::write(
            &wordroot,
            r#"{"gam":{"meaning":"to marry","class":"root","example":["bigamy","monogamy"],"origin":"Greek"}}"#,
        )
        .unwrap();
        let out = build_etymology_dataset(&wordroot, "1.0.0", &tmp.join("ety-out")).unwrap();
        let base = tmp.join("datasets").join(ETY_DATASET_ID);
        std::fs::create_dir_all(base.join("1.0.0")).unwrap();
        for entry in std::fs::read_dir(&out.package_dir).unwrap().flatten() {
            std::fs::copy(
                entry.path(),
                base.join("1.0.0").join(entry.file_name()),
            )
            .unwrap();
        }
        std::fs::write(base.join("active.json"), "{\"version\":\"1.0.0\"}").unwrap();

        // 重新查询：词源包就绪后词条带 gam 词根（bigamy 未收录词条——词源只增强
        // 已收录词，不新增词条）
        let (entry, _) = svc.lookup("monogamy").unwrap();
        let entry = entry.expect("monogamy 应命中");
        let ety = &entry.etymology;
        assert_eq!(ety.len(), 1);
        assert_eq!(ety[0].root, "gam");
        assert_eq!(ety[0].meaning, "to marry");
        assert_eq!(ety[0].origin, "Greek");

        // 无词根记录的词保持空
        let (entry, _) = svc.lookup("apple").unwrap();
        assert!(entry.unwrap().etymology.is_empty());
    }

    #[test]
    fn edit_distance_bounds() {
        assert_eq!(edit_distance_at_most("aple", "apple", 1), Some(1));
        assert_eq!(edit_distance_at_most("aple", "apple", 0), None);
        assert_eq!(edit_distance_at_most("recieve", "receive", 2), Some(2));
        assert_eq!(edit_distance_at_most("abcdefgh", "abcd", 2), None);
    }
}
