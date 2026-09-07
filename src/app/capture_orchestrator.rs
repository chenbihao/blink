//! 截图原子事务编排层（0.22.14）——AI 净化截图的 capture visibility guard。
//!
//! **职责**：在一次截图调用内完成 Blink 窗口净化、目标窗口状态准备、
//! DWM flush、截图、恢复——无论成功/失败/超时/取消均恢复现场。
//!
//! **铁则**：
//! - 不调用 `hide_chat_window`（会 abort ChatService）
//! - 不触发 ChatService abort
//! - 不使用前端 DOM 隐藏
//! - 不把 Blink WebviewWindow 的逻辑 visible 状态改成 false
//! - 只恢复本 guard 实际改变的状态（原先已 cloaked/minimized 的窗口不被错误展开）
//! - 截图失败也必须恢复
//! - 外部窗口目标准备：如果目标最小化则 restore + activate，截图后恢复最小化
//! - include 只表示不隐藏 Blink，不代表跳过外部目标窗口的 restore/activate
//! - **激活失败立即返回 ActivationFailed，不继续截图**
//! - **恢复顺序：先 un-cloak Blink → 恢复目标最小化 → 恢复前台焦点**
//!   （原前台是 Blink 时先解除 cloak 再恢复焦点）
//! - 恢复调用检查结果，失败返回 RestoreFailed
//! - 正常路径通过显式 finalize 返回恢复结果，Drop 作为最后保险
//!
//! **分层**：app 编排层，消费 `infra/platform/window` Win32 原语。
//! 不把编排逻辑堆进 domain capability。

use crate::infra::platform::window as win;

// ── CleansePlan（与 domain policy.rs 的 CaptureCleansePlan 对应）─────────────

/// 截图净化策略——解析 `blink_visibility` + 目标是否 Blink 后的最终执行计划。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleansePlan {
    /// 不操作 Blink 窗口（include）
    Noop,
    /// cloak 全部 Blink 窗口
    CloakAllBlink,
    /// cloak 除目标外的 Blink 窗口（auto + Blink 目标）
    CloakOthersExceptTarget,
}

/// guard 构造错误——区分激活失败和其他失败。
#[derive(Debug, Clone)]
pub enum GuardError {
    /// 目标窗口激活失败——不返回可能被遮挡的伪成功截图。
    ActivationFailed(String),
    /// 其他构造错误。
    #[allow(dead_code)]
    Other(String),
}

/// 记录 guard 实际改变的状态，finalize/Drop 时恢复。
struct ChangedState {
    /// 本次 cloak 的 Blink HWND（需要 un-cloak 恢复）
    cloaked_hwnds: Vec<isize>,
    /// 目标窗口原本最小化 → 截图后恢复最小化
    target_was_minimized: bool,
    target_hwnd: Option<isize>,
    /// 原前台窗口 → 恢复前台焦点
    original_foreground: Option<isize>,
    /// 是否已通过 finalize 显式恢复（避免 Drop 重复恢复）
    finalized: bool,
}

impl ChangedState {
    fn new() -> Self {
        Self {
            cloaked_hwnds: Vec::new(),
            target_was_minimized: false,
            target_hwnd: None,
            original_foreground: None,
            finalized: false,
        }
    }

    /// 执行恢复——按正确顺序恢复所有被改变的状态。
    ///
    /// **恢复顺序**（铁则）：
    /// 1. un-cloak 本次 cloak 的 Blink 窗口（先解除 cloak，再恢复焦点）
    /// 2. 恢复目标窗口最小化
    /// 3. 恢复前台焦点（此时 Blink 已可见，可安全恢复焦点）
    ///
    /// 如果原前台窗口已失效，安全跳过。
    /// 恢复调用检查结果，失败返回 RestoreFailed。
    fn do_restore(&mut self) -> Result<(), String> {
        let mut errors: Vec<String> = Vec::new();

        // 1. 先 un-cloak Blink 窗口（先解除 cloak，再恢复焦点）
        // 这样原前台是 Blink 时，先解除 cloak 再恢复焦点不会焦点到 cloaked 窗口
        for hwnd in &self.cloaked_hwnds {
            let hwnd_raw = windows::Win32::Foundation::HWND(*hwnd as *mut _);
            crate::infra::platform::window::apply_cloak(hwnd_raw, false);
            // 验证 cloak 已解除
            if win::is_cloaked(hwnd_raw) {
                errors.push(format!("HWND {hwnd} un-cloak 后仍处于 cloaked 状态"));
            }
        }

        // 2. 恢复目标窗口最小化
        if self.target_was_minimized
            && let Some(hwnd) = self.target_hwnd
        {
            // 检查目标窗口是否仍然有效
            if win::is_hwnd_valid(hwnd) {
                win::minimize_window(hwnd);
            }
        }

        // 3. 恢复前台焦点（此时 Blink 已可见）
        if let Some(fg) = self.original_foreground {
            // 检查原前台窗口是否仍然有效
            if win::is_hwnd_valid(fg) {
                win::set_foreground(fg);
                // 验证前台恢复——SetForegroundWindow 可能因系统前台锁定失败
                let current_fg = win::get_foreground();
                if current_fg != Some(fg) {
                    // 前台恢复失败属系统限制，记 warning 但不作为硬错误
                    tracing::warn!(
                        expected = fg,
                        actual = ?current_fg,
                        "前台窗口恢复未成功（Windows 前台锁定限制）"
                    );
                }
            }
            // 原前台已失效 → 安全跳过，不误操作其他窗口
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl Drop for ChangedState {
    fn drop(&mut self) {
        // Drop 作为最后保险——如果未通过 finalize 显式恢复，则在此恢复
        if !self.finalized {
            if let Err(e) = self.do_restore() {
                tracing::warn!(error = %e, "CaptureGuard Drop 恢复失败（最后保险路径）");
            }
        }
    }
}

/// 截图净化 guard——在一次截图调用内管理 Blink 窗口可见性。
///
/// **使用方式**：
/// ```ignore
/// let guard = CaptureGuard::new(&app, plan, target_hwnd)?;
/// // guard 已完成 cloak + DwmFlush + 目标激活
/// let result = do_screenshot(); // 截图
/// guard.finalize()?; // 显式恢复，检查恢复结果
/// // Drop 仍作为最后保险
/// ```
pub struct CaptureGuard {
    state: ChangedState,
}

impl CaptureGuard {
    /// 构造 guard：执行净化准备（cloak + DwmFlush + 目标激活），不恢复（finalize/Drop 时恢复）。
    ///
    /// **参数**：
    /// - `app`: Tauri AppHandle
    /// - `plan`: 净化策略
    /// - `target_hwnd`: 目标窗口 HWND（外部窗口或 Blink 窗口；全屏截图传 None）
    ///
    /// **错误**：
    /// - `GuardError::ActivationFailed`：外部目标窗口激活失败，不继续截图
    /// - `GuardError::Other`：其他构造错误
    pub fn new(
        app: &tauri::AppHandle,
        plan: CleansePlan,
        target_hwnd: Option<isize>,
    ) -> Result<Self, GuardError> {
        let mut state = ChangedState::new();
        state.original_foreground = win::get_foreground();

        // 收集需要 cloak 的 Blink HWND
        let blink_hwnds = win::collect_blink_hwnds();

        match plan {
            CleansePlan::Noop => {
                // include：不操作 Blink 窗口
                // 但外部窗口目标准备仍然需要执行（解耦"Blink 是否隐藏"和"目标窗口准备"）
            }
            CleansePlan::CloakAllBlink => {
                for hwnd in &blink_hwnds {
                    if target_hwnd == Some(*hwnd) {
                        continue;
                    }
                    let hwnd_raw = windows::Win32::Foundation::HWND(*hwnd as *mut _);
                    if !win::is_cloaked(hwnd_raw) {
                        crate::infra::platform::window::apply_cloak(hwnd_raw, true);
                        state.cloaked_hwnds.push(*hwnd);
                    }
                }
            }
            CleansePlan::CloakOthersExceptTarget => {
                for hwnd in &blink_hwnds {
                    if target_hwnd == Some(*hwnd) {
                        continue;
                    }
                    let hwnd_raw = windows::Win32::Foundation::HWND(*hwnd as *mut _);
                    if !win::is_cloaked(hwnd_raw) {
                        crate::infra::platform::window::apply_cloak(hwnd_raw, true);
                        state.cloaked_hwnds.push(*hwnd);
                    }
                }
            }
        }

        // 外部窗口目标准备：无论 visibility 策略为何，都按需 restore/activate 目标
        // include 只表示不隐藏 Blink，不代表跳过外部目标窗口的准备
        if let Some(hwnd) = target_hwnd {
            let target_is_blink = blink_hwnds.contains(&hwnd);
            // 只对非 Blink 目标做 restore/activate（Blink 目标已在 blink_hwnds 中）
            if !target_is_blink {
                let hwnd_raw = windows::Win32::Foundation::HWND(hwnd as *mut _);
                if win::is_minimized(hwnd_raw) {
                    state.target_was_minimized = true;
                    state.target_hwnd = Some(hwnd);
                    win::restore_window(hwnd);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                // activate 目标——验证目标确实处于可截图状态
                if !win::activate_window(hwnd) {
                    // 激活失败——立即返回错误，不继续截图
                    // 先恢复本次已经改变的状态（cloak + restore）
                    let _ = state.do_restore();
                    return Err(GuardError::ActivationFailed(
                        "外部目标窗口激活失败（Windows 前台锁定限制）".into(),
                    ));
                }

                // 短暂等待 DWM 合成——作为合成同步的一部分
                std::thread::sleep(std::time::Duration::from_millis(30));

                // 验证目标确实处于前台（可截图状态）
                let current_fg = win::get_foreground();
                if current_fg != Some(hwnd) {
                    // 目标未成功到达前台——可能被其他窗口遮挡
                    // 先恢复本次已经改变的状态
                    let _ = state.do_restore();
                    return Err(GuardError::ActivationFailed(
                        "目标窗口激活后仍未处于前台，可能被遮挡".into(),
                    ));
                }
            }
        }

        // DwmFlush：确保 cloak 生效后得到新合成帧
        win::dwm_flush();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let _ = app;
        Ok(Self { state })
    }

    /// 显式恢复——正常路径通过此方法返回恢复结果。
    ///
    /// 调用后 Drop 不会重复恢复。
    /// 恢复失败返回 `RestoreFailed` 错误信息。
    pub fn finalize(mut self) -> Result<(), String> {
        let result = self.state.do_restore();
        self.state.finalized = true;
        result
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanse_plan_variants_exist() {
        let _ = CleansePlan::Noop;
        let _ = CleansePlan::CloakAllBlink;
        let _ = CleansePlan::CloakOthersExceptTarget;
    }

    #[test]
    fn changed_state_new_is_empty() {
        let s = ChangedState::new();
        assert!(s.cloaked_hwnds.is_empty());
        assert!(!s.target_was_minimized);
        assert!(s.target_hwnd.is_none());
        assert!(s.original_foreground.is_none());
        assert!(!s.finalized);
    }

    #[test]
    fn guard_error_variants_exist() {
        let _ = GuardError::ActivationFailed("test".into());
        let _ = GuardError::Other("test".into());
    }
}
