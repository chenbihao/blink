//! 会话内 operation 日志回放存储（0.22 日志与诊断收口）。
//!
//! `OperationLogStore` 是一个独立、线程安全的 app managed state，用于在
//! **同一次应用进程内** 回放环境安装和模型安装/修复等 operation 日志。
//!
//! ## 不支持跨应用重启持久化
//!
//! 本 store 只存在于内存中，不写入 SQLite 或磁盘。应用重启后 operation
//! 日志丢失——这是有意的限制。runtime/instance 日志继续以 `ManagedProcess`
//! ring buffer 为真源，不在 operation store 重复保存。
//!
//! ## 双重上限
//!
//! - 每个 operation 最大日志条数：`MAX_LOGS_PER_OPERATION`（500）
//! - 每引擎最大 operation 数：`MAX_OPERATIONS_PER_ENGINE`（20）
//!
//! 超出上限时丢弃最旧条目（FIFO 淘汰）。

use std::collections::HashMap;
use std::sync::Mutex;

use crate::infra::local_engine::runtime::EngineId;

/// 每个 operation 最大日志条数。
const MAX_LOGS_PER_OPERATION: usize = 500;

/// 每引擎最大 operation 数。
const MAX_OPERATIONS_PER_ENGINE: usize = 20;

/// 一条 operation 日志记录——与 `EngineLogDto` 等价的结构化记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationLogEntry {
    /// 引擎 id。
    pub engine_id: String,
    /// 安装操作 id。
    pub operation_id: String,
    /// 序号（在同一 operation_id 内单调递增）。
    pub seq: u64,
    /// 时间戳（RFC 3339）。
    pub timestamp: String,
    /// 日志级别。
    pub level: String,
    /// 文本内容。
    pub text: String,
}

/// 单个 operation 的日志缓冲。
#[derive(Debug, Clone)]
struct OperationBuffer {
    operation_id: String,
    logs: Vec<OperationLogEntry>,
}

/// 会话内 operation 日志存储——按 `engine_id + operation_id` 隔离。
///
/// 线程安全（`Mutex` 保护），作为 `Arc<OperationLogStore>` 共享给
/// `TauriEventPort`（写入）和 IPC command（查询）。
pub struct OperationLogStore {
    inner: Mutex<HashMap<String, Vec<OperationBuffer>>>,
}

impl OperationLogStore {
    /// 创建空 store。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 追加一条 operation 日志。
    ///
    /// 按 `engine_id` → `operation_id` 定位缓冲。新 operation 自动创建缓冲。
    /// 超出 `MAX_LOGS_PER_OPERATION` 时丢弃最旧条目。
    /// 超出 `MAX_OPERATIONS_PER_ENGINE` 时淘汰最旧 operation。
    pub fn append(&self, entry: OperationLogEntry) {
        let mut inner = self.inner.lock().unwrap();
        let ops = inner.entry(entry.engine_id.clone()).or_default();

        // 查找或创建 operation buffer
        let buf = if let Some(pos) = ops
            .iter()
            .position(|b| b.operation_id == entry.operation_id)
        {
            &mut ops[pos]
        } else {
            // 新 operation——检查上限
            if ops.len() >= MAX_OPERATIONS_PER_ENGINE {
                // 淘汰最旧（index 0）
                ops.remove(0);
            }
            ops.push(OperationBuffer {
                operation_id: entry.operation_id.clone(),
                logs: Vec::new(),
            });
            ops.last_mut().unwrap()
        };

        buf.logs.push(entry);
        if buf.logs.len() > MAX_LOGS_PER_OPERATION {
            // 丢弃最旧
            buf.logs.remove(0);
        }
    }

    /// 查询指定引擎的所有 operation 日志，按 operation 分组返回。
    ///
    /// 返回的日志在每个 operation 内按 seq 正序排列。
    /// 不同 operation 之间按 operation 创建顺序排列。
    pub fn query(&self, engine_id: &EngineId) -> Vec<OperationLogEntry> {
        let inner = self.inner.lock().unwrap();
        let Some(ops) = inner.get(engine_id.as_str()) else {
            return Vec::new();
        };

        let mut result = Vec::new();
        for buf in ops {
            for log in &buf.logs {
                result.push(log.clone());
            }
        }
        result
    }

    /// 查询指定引擎指定 operation 的日志。
    pub fn query_operation(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
    ) -> Vec<OperationLogEntry> {
        let inner = self.inner.lock().unwrap();
        let Some(ops) = inner.get(engine_id.as_str()) else {
            return Vec::new();
        };

        ops.iter()
            .find(|b| b.operation_id == operation_id)
            .map(|b| b.logs.clone())
            .unwrap_or_default()
    }

    /// 清除指定引擎的所有 operation 日志。
    #[allow(dead_code)]
    pub fn clear_engine(&self, engine_id: &EngineId) {
        let mut inner = self.inner.lock().unwrap();
        inner.remove(engine_id.as_str());
    }

    /// 返回指定引擎的 operation 数量（用于诊断）。
    #[allow(dead_code)]
    pub fn operation_count(&self, engine_id: &EngineId) -> usize {
        let inner = self.inner.lock().unwrap();
        inner
            .get(engine_id.as_str())
            .map(|ops| ops.len())
            .unwrap_or(0)
    }
}

impl Default for OperationLogStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(engine: &str, op: &str, seq: u64, text: &str) -> OperationLogEntry {
        OperationLogEntry {
            engine_id: engine.to_string(),
            operation_id: op.to_string(),
            seq,
            timestamp: format!("2026-08-29T00:00:0{}Z", seq % 10),
            level: "info".to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn append_and_query_single_operation() {
        let store = OperationLogStore::new();
        let eid = EngineId::new("funasr").unwrap();

        store.append(make_entry("funasr", "op-1", 1, "line 1"));
        store.append(make_entry("funasr", "op-1", 2, "line 2"));

        let logs = store.query(&eid);
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].seq, 1);
        assert_eq!(logs[1].seq, 2);
    }

    #[test]
    fn multiple_operations_isolated_by_operation_id() {
        let store = OperationLogStore::new();
        let eid = EngineId::new("funasr").unwrap();

        store.append(make_entry("funasr", "op-1", 1, "a1"));
        store.append(make_entry("funasr", "op-2", 1, "b1"));
        store.append(make_entry("funasr", "op-1", 2, "a2"));
        store.append(make_entry("funasr", "op-2", 2, "b2"));

        let logs = store.query(&eid);
        assert_eq!(logs.len(), 4);
        // op-1 日志在 op-2 之前（按创建顺序）
        assert_eq!(logs[0].operation_id, "op-1");
        assert_eq!(logs[1].operation_id, "op-1");
        assert_eq!(logs[2].operation_id, "op-2");
        assert_eq!(logs[3].operation_id, "op-2");

        let op1 = store.query_operation(&eid, "op-1");
        assert_eq!(op1.len(), 2);
        assert_eq!(op1[0].text, "a1");
        assert_eq!(op1[1].text, "a2");
    }

    #[test]
    fn max_logs_per_operation_drops_oldest() {
        let store = OperationLogStore::new();
        let eid = EngineId::new("funasr").unwrap();

        // 超过上限
        for i in 0..(MAX_LOGS_PER_OPERATION + 10) {
            store.append(make_entry(
                "funasr",
                "op-1",
                i as u64 + 1,
                &format!("line {}", i),
            ));
        }

        let logs = store.query(&eid);
        assert_eq!(logs.len(), MAX_LOGS_PER_OPERATION);
        // 丢弃了最旧的 10 条，第一条应该是 seq=11
        assert_eq!(logs[0].seq, 11);
    }

    #[test]
    fn max_operations_per_engine_drops_oldest() {
        let store = OperationLogStore::new();
        let eid = EngineId::new("funasr").unwrap();

        // 创建超过上限的 operation
        for i in 0..(MAX_OPERATIONS_PER_ENGINE + 5) {
            store.append(make_entry("funasr", &format!("op-{}", i), 1, "x"));
        }

        let ops_count = store.operation_count(&eid);
        assert_eq!(ops_count, MAX_OPERATIONS_PER_ENGINE);
        // 最旧的 5 个被淘汰，第一个应该是 op-5
        let logs = store.query(&eid);
        assert_eq!(logs[0].operation_id, "op-5");
    }

    #[test]
    fn different_engines_isolated() {
        let store = OperationLogStore::new();
        let funasr = EngineId::new("funasr").unwrap();
        let paddleocr = EngineId::new("paddleocr").unwrap();

        store.append(make_entry("funasr", "op-1", 1, "f1"));
        store.append(make_entry("paddleocr", "op-1", 1, "p1"));

        let f_logs = store.query(&funasr);
        let p_logs = store.query(&paddleocr);
        assert_eq!(f_logs.len(), 1);
        assert_eq!(p_logs.len(), 1);
        assert_eq!(f_logs[0].text, "f1");
        assert_eq!(p_logs[0].text, "p1");
    }

    #[test]
    fn query_nonexistent_engine_returns_empty() {
        let store = OperationLogStore::new();
        let eid = EngineId::new("nonexistent").unwrap();
        let logs = store.query(&eid);
        assert!(logs.is_empty());
    }

    #[test]
    fn query_nonexistent_operation_returns_empty() {
        let store = OperationLogStore::new();
        let eid = EngineId::new("funasr").unwrap();
        store.append(make_entry("funasr", "op-1", 1, "x"));
        let logs = store.query_operation(&eid, "nonexistent-op");
        assert!(logs.is_empty());
    }

    #[test]
    fn cross_source_same_seq_does_not_dedup() {
        // 不同 operation 可以有相同 seq——它们是独立缓冲，不去重
        let store = OperationLogStore::new();
        let eid = EngineId::new("funasr").unwrap();

        store.append(make_entry("funasr", "op-1", 1, "a"));
        store.append(make_entry("funasr", "op-2", 1, "b"));

        let logs = store.query(&eid);
        assert_eq!(logs.len(), 2, "不同 operation 的相同 seq 都保留");
        assert_eq!(logs[0].seq, 1);
        assert_eq!(logs[1].seq, 1);
        assert_ne!(logs[0].operation_id, logs[1].operation_id);
    }
}
