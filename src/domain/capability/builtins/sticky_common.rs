use crate::domain::capability::CapabilityError;
use crate::domain::sticky::StickyError;

pub(super) fn map_sticky_error(error: StickyError) -> CapabilityError {
    match error {
        StickyError::Db { detail } => CapabilityError::Internal { detail },
        StickyError::NotFound { id } => CapabilityError::NotFound { id },
        StickyError::Trashed { id } => CapabilityError::InvalidState {
            detail: format!("便签 {id} 已在回收站"),
        },
        StickyError::Conflict {
            id,
            expected_updated_at,
            actual_updated_at,
        } => CapabilityError::Conflict {
            detail: format!(
                "便签 {id} 已被修改（期望版本 {expected_updated_at}，当前版本 {actual_updated_at}）"
            ),
        },
    }
}
