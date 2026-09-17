//! ResourceStore 错误类型（0.23.12）。
//!
//! **隐私铁则**：Debug/Display 输出不含绝对路径、文件名正文、资源字节或
//! 转写全文。`detail` 只携带类别与非敏感诊断信息。

use thiserror::Error;

/// 错误类别——稳定字符串，供调用方（Capability/CLI/MCP）分类展示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceErrorKind {
    /// ref 格式无效、空或未知（跨 store / 已被移除）。
    InvalidResourceRef,
    /// ref 已过期（TTL 超时）。
    StaleResourceRef,
    /// open 声明的 use 不在 grant 授予集合内（错误 use 不消耗 one-shot）。
    UseDenied,
    /// 派生请求试图扩大授权（use 子集 / 读取次数 / 复用策略 / TTL 任一越界）。
    PermissionEscalation,
    /// 文件身份变化（size/mtime/identity 不匹配）。
    FileIdentityChanged,
    /// 不是 regular file（目录、symlink、reparse point）。
    NotRegularFile,
    /// 资源预算超限（条目数、累计字节或单项上限）。
    ResourceBudgetExceeded,
    /// 文件不存在或无法访问。
    FileNotFound,
    /// 打开/读取文件 IO 错误。
    IoError,
    /// backing 未实现（Remote）或操作未立项（read_range）。
    UnsupportedBacking,
}

impl ResourceErrorKind {
    /// 稳定字符串表示——用于 serde 和日志。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidResourceRef => "invalid_resource_ref",
            Self::StaleResourceRef => "stale_resource_ref",
            Self::UseDenied => "use_denied",
            Self::PermissionEscalation => "permission_escalation",
            Self::FileIdentityChanged => "file_identity_changed",
            Self::NotRegularFile => "not_regular_file",
            Self::ResourceBudgetExceeded => "resource_budget_exceeded",
            Self::FileNotFound => "file_not_found",
            Self::IoError => "io_error",
            Self::UnsupportedBacking => "unsupported_backing",
        }
    }
}

impl std::fmt::Display for ResourceErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// ResourceStore 错误——稳定类别 + 非敏感诊断信息。
#[derive(Debug, Clone, Error)]
#[error("{kind}")]
pub struct ResourceError {
    pub kind: ResourceErrorKind,
    /// 非敏感诊断信息（不含绝对路径）。
    #[allow(dead_code)]
    pub detail: Option<String>,
}

impl ResourceError {
    pub fn new(kind: ResourceErrorKind) -> Self {
        Self { kind, detail: None }
    }

    pub fn with_detail(kind: ResourceErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: Some(detail.into()),
        }
    }

    /// io::Error 转换——Display 可能含路径，只保留 error kind。
    pub fn from_io(e: std::io::Error) -> Self {
        let kind = if e.kind() == std::io::ErrorKind::NotFound {
            ResourceErrorKind::FileNotFound
        } else {
            ResourceErrorKind::IoError
        };
        Self::with_detail(kind, format!("io error kind: {:?}", e.kind()))
    }
}

impl From<std::io::Error> for ResourceError {
    fn from(e: std::io::Error) -> Self {
        Self::from_io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_as_str_stable() {
        assert_eq!(
            ResourceErrorKind::InvalidResourceRef.as_str(),
            "invalid_resource_ref"
        );
        assert_eq!(
            ResourceErrorKind::StaleResourceRef.as_str(),
            "stale_resource_ref"
        );
        assert_eq!(ResourceErrorKind::UseDenied.as_str(), "use_denied");
        assert_eq!(
            ResourceErrorKind::PermissionEscalation.as_str(),
            "permission_escalation"
        );
        assert_eq!(
            ResourceErrorKind::FileIdentityChanged.as_str(),
            "file_identity_changed"
        );
        assert_eq!(
            ResourceErrorKind::NotRegularFile.as_str(),
            "not_regular_file"
        );
        assert_eq!(
            ResourceErrorKind::ResourceBudgetExceeded.as_str(),
            "resource_budget_exceeded"
        );
        assert_eq!(ResourceErrorKind::FileNotFound.as_str(), "file_not_found");
        assert_eq!(ResourceErrorKind::IoError.as_str(), "io_error");
        assert_eq!(
            ResourceErrorKind::UnsupportedBacking.as_str(),
            "unsupported_backing"
        );
    }

    #[test]
    fn error_display_is_kind_string() {
        let e = ResourceError::new(ResourceErrorKind::StaleResourceRef);
        assert_eq!(e.to_string(), "stale_resource_ref");
    }

    #[test]
    fn error_debug_does_not_contain_path() {
        let e =
            ResourceError::with_detail(ResourceErrorKind::IoError, "some diagnostic without path");
        let debug_str = format!("{e:?}");
        assert!(!debug_str.contains("C:\\Users"));
        assert!(!debug_str.contains("/home/"));
        assert!(debug_str.contains("IoError"));
    }

    #[test]
    fn from_io_not_found() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = ResourceError::from_io(io_err);
        assert_eq!(err.kind, ResourceErrorKind::FileNotFound);
    }

    #[test]
    fn from_io_other() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no access");
        let err = ResourceError::from_io(io_err);
        assert_eq!(err.kind, ResourceErrorKind::IoError);
    }
}
