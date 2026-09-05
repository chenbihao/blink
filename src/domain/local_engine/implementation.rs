//! 单一产品引擎下的内部 implementation 层（0.22.9）。
//!
//! 产品层始终只有一个稳定 engine id（`funasr` / `paddleocr`）；本模块在其
//! 之下建立**编译期受限的内部 implementation 身份、描述符与模型绑定**，
//! 让 Manager 在 start 时从 selected model 解析并冻结实际 implementation。
//!
//! ## 设计铁则
//!
//! - **闭合枚举 identity**：`ImplementationId` 只允许编译期声明的变体，
//!   不接受前端传入 implementation id，不允许字符串构造 executable/URL/
//!   argv/环境变量/路径。
//! - **descriptor 位于 engine descriptor 之下**：implementation 不新增
//!   第二个产品 engine/adapter，只声明 runtime、transport/topology、
//!   install plan 受限引用、可承载模型与资源预算。
//! - **模型绑定显式且 fail-closed**：每个模型必须显式绑定一个 implementation；
//!   未注册模型不静默换模，绑定矛盾在注册表构造期拒绝。
//! - **只服务编译期内置实现**：不演化为任意第三方 runtime 注册或前端可
//!   注入通道（无动态注册 API）。
//!
//! ## 分层归属
//!
//! - 本模块只做类型、校验与纯解析逻辑，不依赖 infra/tauri/windows。
//! - 具体 builtin descriptor 与绑定表在 app 层装配（需要 infra 的
//!   asset lock artifact id），构造期执行本模块的 fail-closed 校验。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::descriptor::{EngineTimeouts, InstallPlanRef, ResourceBudget, ServiceTransport};
use super::error::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};
use super::identity::EngineId;

// ── ImplementationId ───────────────────────────────────────────────────────

/// 内部 implementation 稳定标识（编译期闭合枚举，0.22.9）。
///
/// 产品层 engine id 保持稳定；同一 engine 内部按模型/技术底座区分
/// implementation。命名稳定且版本化演进靠 revision 声明（descriptor 的
/// install plan artifact id / engine display version），不改 id。
///
/// **不接受前端提交**：serde 反序列化只接受下列 wire 值，未知值直接失败
/// （fail-closed，不用默认值掩盖未知数据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationId {
    /// FunASR GGUF 常驻 worker（llama.cpp 锁定构建，NDJSON stdio 协议）。
    ///
    /// 承载 SenseVoice / Paraformer-zh / Fun-ASR-Nano 三条既有模型路径。
    FunasrGgufWorker,
    /// PaddleOCR ONNX in-process（blink.exe 直持 ORT lazy Session）。
    // 显式 rename：`rename_all` 会把 `PaddleOcr` 拆成 `paddle_ocr`，
    // wire 值必须与产品 engine id 前缀（paddleocr）一致。
    #[serde(rename = "paddleocr_onnx_in_process")]
    PaddleOcrOnnxInProcess,
}

impl ImplementationId {
    /// 返回 wire 值（与 serde 序列化一致，用于日志与诊断）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FunasrGgufWorker => "funasr_gguf_worker",
            Self::PaddleOcrOnnxInProcess => "paddleocr_onnx_in_process",
        }
    }

    /// 从 wire 值解析（未知值返回 `None`——fail-closed，不猜、不用默认值）。
    ///
    /// 用于磁盘目录名（`impl-{wire}`）反解 implementation；磁盘上出现的
    /// 未知名字不是合法 implementation，调用方必须显式处理而非映射默认。
    pub fn parse_wire(value: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(value.to_string())).ok()
    }
}

impl std::fmt::Display for ImplementationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── ExecutorTopology ───────────────────────────────────────────────────────

/// executor topology（0.22.9 §3.10：ONNX Session / runtime 在哪里运行）。
///
/// 只描述执行拓扑，不进入用户模型选择，也不改变 domain 的 STT/OCR port。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorTopology {
    /// 进程内执行：blink.exe 直接持有 runtime/Session（OCR ONNX）。
    InProcess,
    /// 独立受管子进程：worker 持有 runtime，经受限 IPC 调用（GGUF worker）。
    ManagedWorker,
}

impl std::fmt::Display for ExecutorTopology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InProcess => f.write_str("in_process"),
            Self::ManagedWorker => f.write_str("managed_worker"),
        }
    }
}

// ── ImplementationDescriptor ──────────────────────────────────────────────

/// implementation 描述符（静态事实声明，编译期内置）。
///
/// 位于 engine descriptor 之下：同一 engine 的多个 implementation 各自
/// 声明 runtime 种类、服务传输、executor 拓扑、安装计划受限引用、
/// 可承载模型与资源预算。**不暴露可执行路径/argv/env/URL**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationDescriptor {
    /// implementation 身份（编译期闭合枚举）。
    pub id: ImplementationId,
    /// 所属产品 engine id（implementation 只能承载本 engine 的模型）。
    pub engine_id: EngineId,
    /// 运行时种类（闭合枚举，决定使用哪个 provider）。
    pub runtime_kind: super::identity::RuntimePlan,
    /// 服务业务面传输方式（闭合枚举；in-process 引擎为 `InProcess`）。
    pub service_transport: ServiceTransport,
    /// executor 拓扑（in-process / managed worker）。
    pub executor_topology: ExecutorTopology,
    /// 安装计划的受限引用（引用 provider 管理的 artifact，不持有路径）。
    pub install_plan: InstallPlanRef,
    /// 可承载的模型 id 列表（与模型绑定表一致；计划项可为空）。
    pub carried_models: Vec<String>,
    /// 资源预算提示。
    pub resource_budget: ResourceBudget,
    /// 生命周期/超时覆盖（None = 使用 engine descriptor 默认值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<EngineTimeouts>,
}

// ── ImplementationBinding ─────────────────────────────────────────────────

/// 模型 → implementation 的静态绑定（编译期声明）。
///
/// 每个模型**至多**绑定一个 implementation；用户选模型后由 Manager 从
/// 本表解析，不接受前端提交 runtime/transport/implementation。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplementationBinding {
    /// 模型所属的产品 engine id。
    pub engine_id: EngineId,
    /// 稳定模型 id（与模型目录/model descriptor 的 model_id 一致）。
    pub model_id: String,
    /// 绑定的 implementation。
    pub implementation: ImplementationId,
}

// ── ImplementationRegistry ────────────────────────────────────────────────

/// 编译期 implementation 注册表（allowlist，无动态注册 API）。
///
/// 构造时执行 fail-closed 校验（重复 id、跨 engine 绑定、引用未声明的
/// runtime/install plan、同一模型多绑定等），任何矛盾在构造期报错，
/// 不留到首次 start 才发现。
///
/// `Clone` 供 start 冻结段在 `spawn_blocking` 内做纯解析（只读，无共享可变状态）。
#[derive(Debug, Default, Clone)]
pub struct ImplementationRegistry {
    implementations: HashMap<ImplementationId, ImplementationDescriptor>,
    /// (engine_id, model_id) → implementation（模型绑定唯一真源）。
    bindings: HashMap<(EngineId, String), ImplementationId>,
}

impl ImplementationRegistry {
    /// 创建注册表并执行 fail-closed 校验。
    ///
    /// 至少拒绝：
    /// - 同一 implementation id 重复声明；
    /// - implementation 引用未声明的 runtime/install plan（runtime_kind 不一致、
    ///   候选 profile 引用未声明 artifact）；
    /// - binding 引用不存在的 implementation；
    /// - binding 声明属于错误 engine 的模型（binding.engine_id ≠ implementation
    ///   的 engine，或模型不在 implementation 可承载列表中）；
    /// - 同一模型绑定多个 implementation；
    /// - runtime/transport/topology 自相矛盾（in-process 引擎不允许 worker 通道，
    ///   worker 不允许 in-process 通道）；
    /// - 有可承载模型却未声明 install plan artifact（计划项除外）。
    pub fn new_validated(
        implementations: Vec<ImplementationDescriptor>,
        bindings: Vec<ImplementationBinding>,
    ) -> Result<Self, LocalEngineError> {
        let mut map: HashMap<ImplementationId, ImplementationDescriptor> = HashMap::new();
        for desc in implementations {
            let id = desc.id;
            if map.insert(id, desc).is_some() {
                return Err(invalid_config(
                    "implementation id 重复声明",
                    format!("implementation '{id}' 在编译期声明中出现两次"),
                ));
            }
        }

        // implementation 自身一致性
        for desc in map.values() {
            validate_implementation(desc)?;
        }

        // 绑定校验
        let mut bindings_map: HashMap<(EngineId, String), ImplementationId> = HashMap::new();
        for binding in bindings {
            let desc = map.get(&binding.implementation).ok_or_else(|| {
                invalid_config(
                    "模型绑定引用未知 implementation",
                    format!(
                        "模型 '{}' 绑定的 implementation '{}' 不在编译期声明中",
                        binding.model_id, binding.implementation
                    ),
                )
            })?;
            if desc.engine_id != binding.engine_id {
                return Err(invalid_config(
                    "模型绑定跨 engine",
                    format!(
                        "模型 '{}' 声明属于 engine '{}'，但绑定的 implementation '{}' 属于 engine '{}'",
                        binding.model_id, binding.engine_id, binding.implementation, desc.engine_id
                    ),
                ));
            }
            if !desc.carried_models.iter().any(|m| m == &binding.model_id) {
                return Err(invalid_config(
                    "模型不在 implementation 可承载列表中",
                    format!(
                        "implementation '{}' 未声明可承载模型 '{}'",
                        binding.implementation, binding.model_id
                    ),
                ));
            }
            let key = (binding.engine_id.clone(), binding.model_id.clone());
            if bindings_map.insert(key, binding.implementation).is_some() {
                return Err(invalid_config(
                    "同一模型绑定多个 implementation",
                    format!(
                        "engine '{}' 的模型 '{}' 被绑定到多个 implementation",
                        binding.engine_id, binding.model_id
                    ),
                ));
            }
        }

        Ok(Self {
            implementations: map,
            bindings: bindings_map,
        })
    }

    /// 解析模型绑定的 implementation（fail-closed，唯一真源）。
    ///
    /// - 引擎在注册表中有 implementation/绑定声明时：模型必须在绑定表中，
    ///   否则返回 `Err`（**不回退默认 implementation**，未知旧模型保持不可用）；
    /// - 引擎完全无 implementation 声明时：返回 `Ok(None)`
    ///   （implementation 层不适用——仅测试 fake 场景；生产引擎的绑定
    ///   完备性由构造期校验与 app 层测试保证）。
    pub fn resolve_for_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Result<Option<ImplementationId>, LocalEngineError> {
        let engine_declared = self
            .implementations
            .values()
            .any(|d| d.engine_id == *engine_id);
        if !engine_declared {
            return Ok(None);
        }
        self.bindings
            .get(&(engine_id.clone(), model_id.to_string()))
            .copied()
            .map(Some)
            .ok_or_else(|| {
                invalid_config(
                    "模型未绑定本地 implementation",
                    format!(
                        "engine '{engine_id}' 的模型 '{model_id}' 不在编译期绑定表中，\
                         不静默换模"
                    ),
                )
            })
    }

    /// 查找 implementation 描述符。
    pub fn descriptor(&self, id: ImplementationId) -> Option<&ImplementationDescriptor> {
        self.implementations.get(&id)
    }

    /// 返回某 engine 声明的 implementation 列表（按 id 排序，稳定输出）。
    pub fn implementations_for_engine(&self, engine_id: &EngineId) -> Vec<ImplementationId> {
        let mut ids: Vec<ImplementationId> = self
            .implementations
            .values()
            .filter(|d| d.engine_id == *engine_id)
            .map(|d| d.id)
            .collect();
        ids.sort();
        ids
    }

    /// 已声明的 implementation 数量（测试用）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.implementations.len()
    }
}

fn invalid_config(hint: &str, detail: String) -> LocalEngineError {
    LocalEngineError::with_detail(
        LocalEngineErrorCode::InvalidConfig,
        ErrorPhase::Config,
        hint,
        detail,
    )
}

/// 单个 implementation descriptor 的 fail-closed 校验。
fn validate_implementation(desc: &ImplementationDescriptor) -> Result<(), LocalEngineError> {
    // install plan 引用的 runtime 必须与 implementation 声明一致
    if desc.install_plan.runtime_kind != desc.runtime_kind {
        return Err(invalid_config(
            "implementation 配置不一致",
            format!(
                "implementation '{}' install_plan runtime_kind ({}) != 声明 runtime_kind ({})",
                desc.id, desc.install_plan.runtime_kind, desc.runtime_kind
            ),
        ));
    }

    // 候选 profile 引用的 artifact 必须在 install plan 中声明
    for candidate in &desc.install_plan.compute_candidates {
        if !desc
            .install_plan
            .artifact_ids
            .contains(&candidate.artifact_id)
        {
            return Err(invalid_config(
                "implementation 配置不一致",
                format!(
                    "implementation '{}' 候选 profile '{}' 引用未声明的 artifact '{}'",
                    desc.id, candidate.profile_id, candidate.artifact_id
                ),
            ));
        }
    }

    // transport 与 topology 自相矛盾拒绝
    match (desc.executor_topology, desc.service_transport) {
        (ExecutorTopology::InProcess, ServiceTransport::InProcess) => {}
        (ExecutorTopology::ManagedWorker, ServiceTransport::StdioWorker)
        | (ExecutorTopology::ManagedWorker, ServiceTransport::Http) => {}
        (topology, transport) => {
            return Err(invalid_config(
                "runtime/transport/topology 自相矛盾",
                format!(
                    "implementation '{}' topology={} 与 transport={} 不兼容",
                    desc.id, topology, transport
                ),
            ));
        }
    }

    // 可承载模型去重
    let mut seen = std::collections::HashSet::new();
    for model in &desc.carried_models {
        if !seen.insert(model.as_str()) {
            return Err(invalid_config(
                "implementation 可承载模型重复",
                format!("implementation '{}' 重复声明模型 '{}'", desc.id, model),
            ));
        }
    }

    // 有可承载模型就必须声明 install plan artifact（计划项允许全空）
    if !desc.carried_models.is_empty() && desc.install_plan.artifact_ids.is_empty() {
        return Err(invalid_config(
            "implementation 未声明安装计划",
            format!(
                "implementation '{}' 声明了可承载模型但 install_plan 无 artifact",
                desc.id
            ),
        ));
    }

    Ok(())
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::local_engine::identity::{ArtifactId, ComputePreference, RuntimePlan};

    const ENGINE_A: &str = "funasr";
    const ENGINE_B: &str = "paddleocr";

    fn engine(id: &str) -> EngineId {
        EngineId::new(id).unwrap()
    }

    fn artifact(id: &str) -> ArtifactId {
        ArtifactId::new(id).unwrap()
    }

    /// GGUF worker 形态的 descriptor（ManagedBinary + StdioWorker）。
    fn gguf_impl(
        engine: &EngineId,
        id: ImplementationId,
        models: &[&str],
    ) -> ImplementationDescriptor {
        let artifact = artifact("gguf-worker-test");
        ImplementationDescriptor {
            id,
            engine_id: engine.clone(),
            runtime_kind: RuntimePlan::ManagedBinary,
            service_transport: ServiceTransport::StdioWorker,
            executor_topology: ExecutorTopology::ManagedWorker,
            install_plan: InstallPlanRef {
                runtime_kind: RuntimePlan::ManagedBinary,
                artifact_ids: vec![artifact],
                compute_candidates: Vec::new(),
                schema_version: 1,
            },
            carried_models: models.iter().map(|m| m.to_string()).collect(),
            resource_budget: ResourceBudget::default(),
            timeouts: None,
        }
    }

    /// in-process 形态的 descriptor（OnnxRuntime + InProcess）。
    fn inprocess_impl(
        engine: &EngineId,
        id: ImplementationId,
        models: &[&str],
    ) -> ImplementationDescriptor {
        let artifact = artifact("ort-dll-test");
        ImplementationDescriptor {
            id,
            engine_id: engine.clone(),
            runtime_kind: RuntimePlan::OnnxRuntime,
            service_transport: ServiceTransport::InProcess,
            executor_topology: ExecutorTopology::InProcess,
            install_plan: InstallPlanRef {
                runtime_kind: RuntimePlan::OnnxRuntime,
                artifact_ids: vec![artifact],
                compute_candidates: Vec::new(),
                schema_version: 1,
            },
            carried_models: models.iter().map(|m| m.to_string()).collect(),
            resource_budget: ResourceBudget::default(),
            timeouts: None,
        }
    }

    fn binding(
        engine_id: &str,
        model: &str,
        implementation: ImplementationId,
    ) -> ImplementationBinding {
        ImplementationBinding {
            engine_id: engine(engine_id),
            model_id: model.to_string(),
            implementation,
        }
    }

    // ── identity 闭合性 ─────────────────────────────────────────────────────

    #[test]
    fn implementation_id_wire_values_stable() {
        // wire 值是诊断/状态投影契约，只能追加不能修改
        assert_eq!(
            serde_json::to_string(&ImplementationId::FunasrGgufWorker).unwrap(),
            "\"funasr_gguf_worker\""
        );
        assert_eq!(
            serde_json::to_string(&ImplementationId::PaddleOcrOnnxInProcess).unwrap(),
            "\"paddleocr_onnx_in_process\""
        );
        // Display 与 wire 值一致
        assert_eq!(
            ImplementationId::FunasrGgufWorker.to_string(),
            "funasr_gguf_worker"
        );
    }

    #[test]
    fn implementation_id_rejects_unknown_wire_value() {
        // 闭合枚举：未知值拒绝，不能用字符串构造/注入 implementation id
        assert!(serde_json::from_str::<ImplementationId>("\"custom-worker\"").is_err());
        assert!(serde_json::from_str::<ImplementationId>("\"funasr\"").is_err());
        assert!(serde_json::from_str::<ImplementationId>("\"\"").is_err());
    }

    #[test]
    fn parse_wire_roundtrip_and_fail_closed() {
        // wire 值 ↔ 枚举 双向一致（磁盘目录名反解的真源）
        for id in [
            ImplementationId::FunasrGgufWorker,
            ImplementationId::PaddleOcrOnnxInProcess,
        ] {
            assert_eq!(ImplementationId::parse_wire(id.as_str()), Some(id));
        }
        // 未知名字 fail-closed：返回 None，不映射默认 implementation
        assert_eq!(ImplementationId::parse_wire("custom-worker"), None);
        assert_eq!(ImplementationId::parse_wire(""), None);
        assert_eq!(ImplementationId::parse_wire("funasr"), None);
    }

    // ── 合法构造与解析 ─────────────────────────────────────────────────────

    #[test]
    fn valid_registry_resolves_models() {
        let registry = ImplementationRegistry::new_validated(
            vec![
                gguf_impl(
                    &engine(ENGINE_A),
                    ImplementationId::FunasrGgufWorker,
                    &["model-sv", "model-pf", "model-nano"],
                ),
                inprocess_impl(
                    &engine(ENGINE_B),
                    ImplementationId::PaddleOcrOnnxInProcess,
                    &["ocr-model"],
                ),
            ],
            vec![
                binding(ENGINE_A, "model-sv", ImplementationId::FunasrGgufWorker),
                binding(ENGINE_A, "model-pf", ImplementationId::FunasrGgufWorker),
                binding(ENGINE_A, "model-nano", ImplementationId::FunasrGgufWorker),
                binding(
                    ENGINE_B,
                    "ocr-model",
                    ImplementationId::PaddleOcrOnnxInProcess,
                ),
            ],
        )
        .expect("合法声明应通过校验");

        // 三个模型都解析到同一 implementation
        for model in ["model-sv", "model-pf", "model-nano"] {
            assert_eq!(
                registry
                    .resolve_for_model(&engine(ENGINE_A), model)
                    .expect("已声明模型应解析成功"),
                Some(ImplementationId::FunasrGgufWorker)
            );
        }
        // OCR 模型解析到 in-process implementation
        assert_eq!(
            registry
                .resolve_for_model(&engine(ENGINE_B), "ocr-model")
                .expect("已声明模型应解析成功"),
            Some(ImplementationId::PaddleOcrOnnxInProcess)
        );
    }

    #[test]
    fn unknown_model_is_not_silently_bound_to_default() {
        let registry = ImplementationRegistry::new_validated(
            vec![gguf_impl(
                &engine(ENGINE_A),
                ImplementationId::FunasrGgufWorker,
                &["model-sv"],
            )],
            vec![binding(
                ENGINE_A,
                "model-sv",
                ImplementationId::FunasrGgufWorker,
            )],
        )
        .expect("合法声明应通过校验");

        // 未知模型 fail-closed：不回退默认 implementation
        let err = registry
            .resolve_for_model(&engine(ENGINE_A), "legacy-unknown-model")
            .expect_err("未知模型必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn engine_without_implementations_resolves_to_none() {
        // 引擎完全无 implementation 声明（测试 fake 场景）→ Ok(None)
        let registry = ImplementationRegistry::new_validated(
            vec![gguf_impl(
                &engine(ENGINE_A),
                ImplementationId::FunasrGgufWorker,
                &["model-sv"],
            )],
            vec![binding(
                ENGINE_A,
                "model-sv",
                ImplementationId::FunasrGgufWorker,
            )],
        )
        .expect("合法声明应通过校验");

        assert_eq!(
            registry
                .resolve_for_model(&engine("other-engine"), "any-model")
                .expect("无声明的引擎返回 None"),
            None
        );
    }

    #[test]
    fn cross_engine_resolution_fails() {
        // engine B 的模型不能用 engine A 的绑定解析
        let registry = ImplementationRegistry::new_validated(
            vec![
                gguf_impl(
                    &engine(ENGINE_A),
                    ImplementationId::FunasrGgufWorker,
                    &["model-sv"],
                ),
                inprocess_impl(
                    &engine(ENGINE_B),
                    ImplementationId::PaddleOcrOnnxInProcess,
                    &["ocr-model"],
                ),
            ],
            vec![
                binding(ENGINE_A, "model-sv", ImplementationId::FunasrGgufWorker),
                binding(
                    ENGINE_B,
                    "ocr-model",
                    ImplementationId::PaddleOcrOnnxInProcess,
                ),
            ],
        )
        .expect("合法声明应通过校验");

        assert!(
            registry
                .resolve_for_model(&engine(ENGINE_B), "model-sv")
                .is_err()
        );
        assert!(
            registry
                .resolve_for_model(&engine(ENGINE_A), "ocr-model")
                .is_err()
        );
    }

    // ── 构造期 fail-closed 校验 ─────────────────────────────────────────────

    #[test]
    fn duplicate_implementation_id_rejected() {
        let err = ImplementationRegistry::new_validated(
            vec![
                gguf_impl(
                    &engine(ENGINE_A),
                    ImplementationId::FunasrGgufWorker,
                    &["a"],
                ),
                gguf_impl(
                    &engine(ENGINE_A),
                    ImplementationId::FunasrGgufWorker,
                    &["b"],
                ),
            ],
            vec![],
        )
        .expect_err("重复 implementation id 必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn binding_to_unknown_implementation_rejected() {
        let err = ImplementationRegistry::new_validated(
            vec![inprocess_impl(
                &engine(ENGINE_B),
                ImplementationId::PaddleOcrOnnxInProcess,
                &["ocr-model"],
            )],
            vec![binding(
                ENGINE_A,
                "model-sv",
                ImplementationId::FunasrGgufWorker,
            )],
        )
        .expect_err("绑定引用不存在的 implementation 必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn binding_engine_mismatch_rejected() {
        // binding 声明 engine A，但 implementation 属于 engine A、模型不在
        // 可承载列表（跨 engine 绑定的一种）→ 拒绝
        let err = ImplementationRegistry::new_validated(
            vec![gguf_impl(
                &engine(ENGINE_A),
                ImplementationId::FunasrGgufWorker,
                &["model-sv"],
            )],
            vec![binding(
                ENGINE_B,
                "model-sv",
                ImplementationId::FunasrGgufWorker,
            )],
        )
        .expect_err("跨 engine 绑定必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn model_not_in_carried_list_rejected() {
        let err = ImplementationRegistry::new_validated(
            vec![gguf_impl(
                &engine(ENGINE_A),
                ImplementationId::FunasrGgufWorker,
                &["model-sv"],
            )],
            vec![binding(
                ENGINE_A,
                "other-model",
                ImplementationId::FunasrGgufWorker,
            )],
        )
        .expect_err("模型不在可承载列表必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn duplicate_model_binding_rejected() {
        let err = ImplementationRegistry::new_validated(
            vec![
                gguf_impl(
                    &engine(ENGINE_A),
                    ImplementationId::FunasrGgufWorker,
                    &["model-sv"],
                ),
                inprocess_impl(
                    &engine(ENGINE_A),
                    ImplementationId::PaddleOcrOnnxInProcess,
                    &["model-sv"],
                ),
            ],
            vec![
                binding(ENGINE_A, "model-sv", ImplementationId::FunasrGgufWorker),
                binding(
                    ENGINE_A,
                    "model-sv",
                    ImplementationId::PaddleOcrOnnxInProcess,
                ),
            ],
        )
        .expect_err("同一模型绑定多个 implementation 必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn duplicate_carried_model_rejected() {
        let err = ImplementationRegistry::new_validated(
            vec![gguf_impl(
                &engine(ENGINE_A),
                ImplementationId::FunasrGgufWorker,
                &["model-sv", "model-sv"],
            )],
            vec![],
        )
        .expect_err("可承载模型重复必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn install_plan_runtime_mismatch_rejected() {
        let mut desc = gguf_impl(
            &engine(ENGINE_A),
            ImplementationId::FunasrGgufWorker,
            &["model-sv"],
        );
        desc.install_plan.runtime_kind = RuntimePlan::OnnxRuntime;
        let err = ImplementationRegistry::new_validated(vec![desc], vec![])
            .expect_err("install plan runtime 与声明不一致必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn candidate_referencing_undeclared_artifact_rejected() {
        use crate::domain::local_engine::descriptor::ComputeCandidate;
        let mut desc = gguf_impl(
            &engine(ENGINE_A),
            ImplementationId::FunasrGgufWorker,
            &["model-sv"],
        );
        desc.install_plan.compute_candidates.push(ComputeCandidate {
            preference: ComputePreference::Cpu,
            profile_id: "cpu-x64".to_string(),
            artifact_id: artifact("undeclared-artifact"),
        });
        let err = ImplementationRegistry::new_validated(vec![desc], vec![])
            .expect_err("候选引用未声明 artifact 必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn topology_transport_contradiction_rejected() {
        // in-process topology + stdio worker transport → 矛盾
        let mut desc = inprocess_impl(
            &engine(ENGINE_B),
            ImplementationId::PaddleOcrOnnxInProcess,
            &["ocr-model"],
        );
        desc.service_transport = ServiceTransport::StdioWorker;
        let err = ImplementationRegistry::new_validated(vec![desc], vec![])
            .expect_err("topology 与 transport 矛盾必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);

        // worker topology + in-process transport → 矛盾
        let mut desc = gguf_impl(
            &engine(ENGINE_A),
            ImplementationId::FunasrGgufWorker,
            &["model-sv"],
        );
        desc.service_transport = ServiceTransport::InProcess;
        let err = ImplementationRegistry::new_validated(vec![desc], vec![])
            .expect_err("worker 引擎不允许 in-process 通道");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn carried_models_without_install_artifacts_rejected() {
        let mut desc = gguf_impl(
            &engine(ENGINE_A),
            ImplementationId::FunasrGgufWorker,
            &["model-sv"],
        );
        desc.install_plan.artifact_ids.clear();
        let err = ImplementationRegistry::new_validated(vec![desc], vec![])
            .expect_err("有承载模型却无 install artifact 必须拒绝");
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn planned_implementation_without_models_is_allowed() {
        // 内部计划项：不承载模型、无 install artifact —— 允许声明，
        // 但不得被任何模型绑定（绑定校验会拦截）
        let planned = ImplementationDescriptor {
            id: ImplementationId::PaddleOcrOnnxInProcess,
            engine_id: engine(ENGINE_A),
            runtime_kind: RuntimePlan::OnnxRuntime,
            service_transport: ServiceTransport::StdioWorker,
            executor_topology: ExecutorTopology::ManagedWorker,
            install_plan: InstallPlanRef {
                runtime_kind: RuntimePlan::OnnxRuntime,
                artifact_ids: Vec::new(),
                compute_candidates: Vec::new(),
                schema_version: 1,
            },
            carried_models: Vec::new(),
            resource_budget: ResourceBudget::default(),
            timeouts: None,
        };
        let registry = ImplementationRegistry::new_validated(vec![planned], vec![])
            .expect("内部计划项（无模型无 artifact）应允许声明");
        assert_eq!(registry.len(), 1);
        // 计划项不承载任何模型 → 该引擎解析任何模型都 fail-closed
        assert!(
            registry
                .resolve_for_model(&engine(ENGINE_A), "planned-unbound-model")
                .is_err()
        );
    }

    // ── descriptor 访问 ─────────────────────────────────────────────────────

    #[test]
    fn implementations_for_engine_returns_only_owned() {
        let registry = ImplementationRegistry::new_validated(
            vec![
                gguf_impl(
                    &engine(ENGINE_A),
                    ImplementationId::FunasrGgufWorker,
                    &["model-sv"],
                ),
                inprocess_impl(
                    &engine(ENGINE_B),
                    ImplementationId::PaddleOcrOnnxInProcess,
                    &["ocr-model"],
                ),
            ],
            vec![],
        )
        .expect("合法声明应通过校验");

        assert_eq!(
            registry.implementations_for_engine(&engine(ENGINE_A)),
            vec![ImplementationId::FunasrGgufWorker]
        );
        assert_eq!(
            registry.implementations_for_engine(&engine(ENGINE_B)),
            vec![ImplementationId::PaddleOcrOnnxInProcess]
        );
        assert!(
            registry
                .implementations_for_engine(&engine("other"))
                .is_empty()
        );

        // descriptor 访问
        let desc = registry
            .descriptor(ImplementationId::FunasrGgufWorker)
            .expect("已声明的 implementation 可查询");
        assert_eq!(desc.engine_id, engine(ENGINE_A));
        assert_eq!(desc.runtime_kind, RuntimePlan::ManagedBinary);
    }

    // ── serde 兼容 ──────────────────────────────────────────────────────────

    #[test]
    fn executor_topology_wire_values_stable() {
        assert_eq!(
            serde_json::to_string(&ExecutorTopology::InProcess).unwrap(),
            "\"in_process\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutorTopology::ManagedWorker).unwrap(),
            "\"managed_worker\""
        );
        assert!(serde_json::from_str::<ExecutorTopology>("\"hybrid\"").is_err());
    }

    #[test]
    fn descriptor_roundtrip_preserves_fields() {
        let desc = gguf_impl(
            &engine(ENGINE_A),
            ImplementationId::FunasrGgufWorker,
            &["model-sv"],
        );
        let json = serde_json::to_string(&desc).unwrap();
        let back: ImplementationDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, desc.id);
        assert_eq!(back.engine_id, desc.engine_id);
        assert_eq!(back.carried_models, desc.carried_models);
        assert!(back.timeouts.is_none());
    }
}
