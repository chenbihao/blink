//! 平台快照仅枚举本机；不扫描局域网，不探测或修改路由器。
#[cfg(windows)]
mod windows;

pub fn snapshot(enhanced: bool) -> crate::model::NetworkSnapshot {
    #[cfg(windows)]
    {
        windows::snapshot(enhanced)
    }
    #[cfg(not(windows))]
    {
        let _ = enhanced;
        crate::model::NetworkSnapshot {
            warning: Some("当前平台尚未支持网卡枚举".into()),
            ..Default::default()
        }
    }
}
