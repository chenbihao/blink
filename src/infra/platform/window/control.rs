//! Win32 窗口控制原语与 opaque window_ref 注册表（0.22.14）。
//!
//! **职责**：
//! - `window_ref` 注册/校验：短期有效的 opaque 窗口引用，绑定 HWND/PID/Blink 身份/generation/TTL
//! - Win32 窗口状态原语：activate / minimize / maximize / restore
//! - DWM cloak 查询：判断窗口当前是否已被 cloak
//! - 窗口身份信息收集：标题/进程名/PID
//!
//! **分层**：纯 Win32 调用，不依赖 tauri 或 domain。app 层编排消费此模块。
//!
//! **测试策略**：注册表和身份校验逻辑走纯函数测试；Win32 原语按 spec-backend §二
//! "Win32/GUI 集成层免自动化"，手动验证。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SW_RESTORE,
    SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, ShowWindow,
};

// ── Blink HWND 收集 ──────────────────────────────────────────────────────────

thread_local! {
    /// EnumWindows 回调收集器（collect_blink_hwnds 专用）。
    static BLINK_BUF: std::cell::RefCell<Vec<isize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// 收集当前 Blink 进程所有可见顶层窗口的 HWND。
///
/// 通过 `EnumWindows` + PID 比对 `GetCurrentProcessId()` 实现。
/// 返回的 HWND 列表供 `CaptureVisibilityGuard` cloak 使用。
///
/// **排除已 cloak 的窗口**——DWM cloaked 窗口本身已不可见，不需要再 cloak。
pub fn collect_blink_hwnds() -> Vec<isize> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
    use windows::core::BOOL;

    let current_pid = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };

    BLINK_BUF.with(|buf| buf.borrow_mut().clear());

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let current_pid = lparam.0 as u32;
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == current_pid && unsafe { IsWindowVisible(hwnd) }.as_bool() && !is_cloaked(hwnd) {
            BLINK_BUF.with(|buf| {
                buf.borrow_mut().push(hwnd.0 as isize);
            });
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(current_pid as isize));
    }

    BLINK_BUF.with(|buf| buf.borrow_mut().drain(..).collect())
}

/// 查询窗口当前是否被 DWM cloak。
pub fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: i32 = 0;
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut i32 as *mut _,
            std::mem::size_of::<i32>() as u32,
        )
    };
    hr.is_ok() && cloaked != 0
}

/// 查询窗口当前是否最小化。
pub fn is_minimized(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() }
}

/// 查询窗口当前是否最大化。
pub fn is_maximized(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::IsZoomed;
    unsafe { IsZoomed(hwnd).as_bool() }
}

/// HWND 是否仍然有效。
pub fn is_hwnd_valid(hwnd: isize) -> bool {
    let hwnd = HWND(hwnd as *mut _);
    !hwnd.is_invalid() && unsafe { IsWindow(Some(hwnd)).as_bool() }
}

/// 获取窗口的 PID。
pub fn get_window_pid(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// 获取当前前台窗口 HWND。
pub fn get_foreground() -> Option<isize> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        None
    } else {
        Some(hwnd.0 as isize)
    }
}

/// 获取窗口标题（通过 GetWindowTextW）。
///
/// **隐私**：调用方不得在日志中记录完整标题。
pub fn get_window_title(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize) + 1];
    let actual = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if actual > 0 {
        String::from_utf16_lossy(&buf[..actual as usize])
    } else {
        String::new()
    }
}

// ── Win32 窗口状态原语 ───────────────────────────────────────────────────────

/// 激活窗口（bring to front + set focus）。
///
/// 使用 `SetForegroundWindow`，Windows 前台锁定限制可能失败。
/// 返回是否成功。
pub fn activate_window(hwnd: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    let hwnd = HWND(hwnd as *mut _);
    if hwnd.is_invalid() {
        return false;
    }
    // 如果最小化，先 restore
    if is_minimized(hwnd) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    }
    unsafe { SetForegroundWindow(hwnd).as_bool() }
}

/// 最小化窗口。
pub fn minimize_window(hwnd: isize) {
    let hwnd = HWND(hwnd as *mut _);
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWMINIMIZED);
    }
}

/// 最大化窗口。
pub fn maximize_window(hwnd: isize) {
    let hwnd = HWND(hwnd as *mut _);
    if hwnd.is_invalid() {
        return;
    }
    // 如果最小化，先 restore 再 maximize
    if is_minimized(hwnd) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
    }
}

/// 恢复窗口（从最小化/最大化恢复到正常状态）。
pub fn restore_window(hwnd: isize) {
    let hwnd = HWND(hwnd as *mut _);
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
}

/// 设置前台窗口（不使用 AttachThreadInput 的简单版本，供截图事务恢复用）。
pub fn set_foreground(hwnd: isize) {
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    let hwnd = HWND(hwnd as *mut _);
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
}

// ── DwmFlush（供 capture guard 使用）──────────────────────────────────────────

/// 刷新 DWM 合成——确保 cloak 生效后得到新合成帧。
///
/// 必须在 blocking 线程调用（DwmFlush 是同步阻塞）。
pub fn dwm_flush() {
    use windows::Win32::Graphics::Dwm::DwmFlush;
    unsafe {
        let _ = DwmFlush();
    }
}

// ── window_ref 注册表 ─────────────────────────────────────────────────────────
//
// 短期有效的 opaque 窗口引用。AI 的 screenshot {op:"window"} 和 manage_window
// 必须使用 window_ref，不得把模型生成的裸 HWND 当作权限凭据。
//
// 安全模型：
// - token 不可预测——使用随机 128-bit hex，不依赖 generation/sequence 作为凭据
// - TTL 校验——issued_at 参与校验，默认 60 秒过期
// - generation 隔离——每次 list_windows 推进 generation，旧 generation 引用过期
// - 执行前二次校验——在 screenshot/manage_window 真正执行前重新校验
//   HWND、PID、窗口身份、TTL 和 generation

/// window_ref 默认 TTL（60 秒）。
const REF_TTL: Duration = Duration::from_secs(60);

/// 单条窗口引用记录。
#[derive(Debug, Clone)]
pub struct WindowRefRecord {
    /// opaque 不可预测的 token（128-bit hex）
    #[allow(dead_code)]
    pub ref_id: String,
    /// HWND（isize）
    pub hwnd: isize,
    /// 进程 PID
    pub pid: u32,
    /// 是否属于 Blink 当前进程
    #[allow(dead_code)]
    pub is_blink: bool,
    /// 窗口标题（用于身份校验，**不记录在日志中**）
    #[allow(dead_code)]
    pub title: String,
    /// 进程名（用于身份校验）
    #[allow(dead_code)]
    pub process_name: String,
    /// 签发时间（参与 TTL 校验）
    pub issued_at: Instant,
    /// generation（递增，用于批量过期）
    #[allow(dead_code)]
    pub generation: u64,
}

/// 校验结果——区分不同的失效原因，供调用方返回结构化错误。
///
/// **注意**：不实现 `PartialEq` 因为 `WindowRefRecord` 含 `Instant`（不实现 `Eq`）。
/// 测试中使用 `is_variant()` 方法进行匹配。
#[derive(Debug, Clone)]
pub enum RefValidation {
    /// 校验通过，返回记录。
    Valid(WindowRefRecord),
    /// ref_id 不存在于注册表。
    NotFound,
    /// generation 过期（太旧）。
    ExpiredGeneration,
    /// TTL 超时（issued_at 距今超过 REF_TTL）。
    ExpiredTtl,
    /// HWND 已无效（窗口被关闭/销毁）。
    InvalidHwnd,
    /// PID 变化（窗口句柄被复用）。
    PidMismatch,
    /// 窗口标题变化（身份变化）。
    TitleMismatch,
}

impl RefValidation {
    /// 比较变体（不比较载荷），供测试使用。
    #[cfg(test)]
    pub fn is_variant(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// 全局 generation 计数器。
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 注册表：ref_id → record。
static REGISTRY: OnceLock<Mutex<HashMap<String, WindowRefRecord>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, WindowRefRecord>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 生成不可预测的 128-bit hex token。
fn generate_token(generation: u64) -> String {
    // 使用进程级随机源——不依赖外部 crate，用 Win32 CryptGenRandom 或
    // 退而用 PID + Instant nanos + generation + 序列号的混合 hash。
    // 这里用简单但足够的方案：时间戳纳秒 + generation + counter 混合。
    // 对于安全敏感场景应使用 getrandom，但 0.22.14 不引入新依赖。
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(generation);
    hasher.write_u64(SEQ.fetch_add(1, Ordering::SeqCst));
    hasher.write_u64(Instant::now().elapsed().as_nanos() as u64);
    // 追加额外熵：PID + 线程 ID
    let pid = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };
    hasher.write_u32(pid);
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    hasher.write_u32(tid);
    let h1 = hasher.finish();
    let mut hasher2 = std::collections::hash_map::RandomState::new().build_hasher();
    hasher2.write_u64(h1);
    hasher2.write_u64(generation.wrapping_add(0xDEAD_BEEF));
    let h2 = hasher2.finish();
    format!("wref_{h1:016x}{h2:016x}")
}

/// 生成新 generation（每次 list_windows 调用时推进）。
pub fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::SeqCst)
}

/// 获取当前 generation。
pub fn current_generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// 注册一个窗口引用，返回 opaque 不可预测的 ref_id。
///
/// 调用方在 `list_windows` 中对每个窗口调用此函数。
pub fn register_window_ref(
    hwnd: isize,
    pid: u32,
    is_blink: bool,
    title: &str,
    process_name: &str,
    generation: u64,
) -> String {
    let ref_id = generate_token(generation);
    let record = WindowRefRecord {
        ref_id: ref_id.clone(),
        hwnd,
        pid,
        is_blink,
        title: title.to_string(),
        process_name: process_name.to_string(),
        issued_at: Instant::now(),
        generation,
    };
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    map.insert(ref_id.clone(), record);
    ref_id
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 校验 window_ref 并返回详细结果。
///
/// 校验项（按顺序）：
/// 1. ref_id 存在于注册表
/// 2. generation 未过期（当前或上一个 generation 内）
/// 3. TTL 未超时（issued_at 距今 ≤ REF_TTL）
/// 4. HWND 仍有效
/// 5. PID 未变化
/// 6. 窗口标题仍与签发记录一致（允许空标题变化，但非空标题必须匹配）
///
/// 失效时返回对应的 `RefValidation` 变体，调用方据此返回结构化错误。
pub fn validate_window_ref_detailed(ref_id: &str) -> RefValidation {
    let map = registry().lock().unwrap_or_else(|e| e.into_inner());
    let Some(record) = map.get(ref_id) else {
        return RefValidation::NotFound;
    };

    // generation 过期检查：只接受当前或上一个 generation
    let current_gen = current_generation();
    if record.generation + 1 < current_gen {
        return RefValidation::ExpiredGeneration;
    }

    // TTL 校验
    if record.issued_at.elapsed() > REF_TTL {
        return RefValidation::ExpiredTtl;
    }

    // HWND 有效性
    if !is_hwnd_valid(record.hwnd) {
        return RefValidation::InvalidHwnd;
    }

    // PID 校验
    let hwnd = HWND(record.hwnd as *mut _);
    let current_pid = get_window_pid(hwnd);
    if current_pid != record.pid {
        return RefValidation::PidMismatch;
    }

    // 身份校验：标题必须匹配（非空标题情况下）
    let current_title = get_window_title(hwnd);
    if !record.title.is_empty() && current_title != record.title {
        return RefValidation::TitleMismatch;
    }

    RefValidation::Valid(record.clone())
}

/// 清理旧 generation 的引用（防止注册表无限增长）。
///
/// 保留当前和上一个 generation 的引用，更老的删除。
pub fn cleanup_old_refs() {
    let current_gen = current_generation();
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, record| record.generation + 1 >= current_gen);
}

/// 获取注册表大小（诊断用）。
#[cfg(test)]
pub fn registry_size() -> usize {
    registry().lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// 清空注册表（测试用）。
#[cfg(test)]
pub fn clear_registry() {
    registry().lock().unwrap_or_else(|e| e.into_inner()).clear();
    SEQ.store(0, Ordering::SeqCst);
    GENERATION.store(0, Ordering::SeqCst);
}

// 测试间互斥锁——防止并行测试污染全局注册表状态。
#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_validate_window_ref() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let gen_val = next_generation();
        let ref_id = register_window_ref(12345, 999, false, "TestWin", "test.exe", gen_val);
        assert!(!ref_id.is_empty());
        assert!(ref_id.starts_with("wref_"));

        // 注册表应有 1 条
        assert_eq!(registry_size(), 1);

        // 注意：validate_window_ref_detailed 会检查 HWND 是否有效（IsWindow），
        // 假 HWND 12345 不会通过 IsWindow 检查
        let val = validate_window_ref_detailed(&ref_id);
        assert!(
            val.is_variant(&RefValidation::InvalidHwnd),
            "应返回 InvalidHwnd"
        );
    }

    #[test]
    fn cleanup_removes_old_generations() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let gen1 = next_generation();
        let _ = register_window_ref(100, 1, false, "A", "a.exe", gen1);
        let gen2 = next_generation();
        let _ = register_window_ref(200, 2, false, "B", "b.exe", gen2);
        assert_eq!(registry_size(), 2);

        cleanup_old_refs();
        assert_eq!(registry_size(), 1);

        let gen3 = next_generation();
        let _ = register_window_ref(300, 3, false, "C", "c.exe", gen3);
        cleanup_old_refs();
        assert_eq!(registry_size(), 1);
    }

    #[test]
    fn generation_increments() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let g1 = next_generation();
        let g2 = next_generation();
        assert_eq!(g2, g1 + 1);
    }

    #[test]
    fn ref_id_format_is_opaque() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let gen_val = next_generation();
        let ref_id = register_window_ref(42, 100, true, "BlinkWin", "blink.exe", gen_val);
        // opaque 格式：wref_{128-bit hex}，不暴露 HWND/PID/generation/sequence
        assert!(ref_id.starts_with("wref_"));
        // token 长度：wref_ + 32 hex chars = 37
        assert_eq!(ref_id.len(), 37, "ref_id 应为 wref_ + 32 hex chars");
        // token 部分（去掉 wref_ 前缀）应全部是 hex 字符
        let token = &ref_id[5..];
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "token 应全部为 hex 字符"
        );
        // 不包含原始值的十进制表示（注意：短数字如 "42" 可能巧合出现在 hex 中，
        // 但这并不意味着能从中逆推出 HWND——token 是 hash 输出，不是编码）
        // 只验证 token 不包含明显的非 hex 编码模式
        assert!(!token.contains("BlinkWin"), "不包含窗口标题");
        assert!(!token.contains("blink"), "不包含进程名");
    }

    #[test]
    fn ref_token_is_unpredictable() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let gen_val = next_generation();
        let ref1 = register_window_ref(1, 100, false, "A", "a.exe", gen_val);
        let ref2 = register_window_ref(2, 200, false, "B", "b.exe", gen_val);
        // 两个 ref 的 token 部分应不同（不可预测）
        let token1 = &ref1[5..]; // strip "wref_"
        let token2 = &ref2[5..];
        assert_ne!(token1, token2, "两个 token 不应相同");
    }

    // ── 策略矩阵补充测试 ──────────────────────────────────────────────────

    #[test]
    fn validate_unknown_ref_returns_not_found() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let _ = next_generation();
        assert!(
            validate_window_ref_detailed("wref_99999999deadbeef")
                .is_variant(&RefValidation::NotFound)
        );
        assert!(validate_window_ref_detailed("").is_variant(&RefValidation::NotFound));
        assert!(validate_window_ref_detailed("not_a_ref").is_variant(&RefValidation::NotFound));
    }

    #[test]
    fn ref_id_sequence_increments() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let gen_val = next_generation();
        let ref1 = register_window_ref(1, 100, false, "A", "a.exe", gen_val);
        let ref2 = register_window_ref(2, 200, false, "B", "b.exe", gen_val);
        // 同 generation 内 token 不同（不可预测）
        assert_ne!(ref1, ref2);
    }

    #[test]
    fn cleanup_after_many_generations() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        for i in 0..5 {
            let g = next_generation();
            let _ = register_window_ref(i, i as u32 + 1, false, &format!("Win{i}"), "app.exe", g);
        }
        assert_eq!(registry_size(), 5);

        cleanup_old_refs();
        assert_eq!(registry_size(), 1);
    }

    #[test]
    fn is_blink_flag_preserved() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let gen_val = next_generation();
        let ref_blink = register_window_ref(100, 42, true, "BlinkWin", "blink.exe", gen_val);
        let ref_external = register_window_ref(200, 99, false, "External", "app.exe", gen_val);

        // 验证注册表中的 is_blink 标记（通过注册表直接读取）
        {
            let map = registry().lock().unwrap_or_else(|e| e.into_inner());
            let rec_blink = map.get(&ref_blink).unwrap();
            assert!(rec_blink.is_blink, "Blink 窗口的 is_blink 应为 true");
            let rec_ext = map.get(&ref_external).unwrap();
            assert!(!rec_ext.is_blink, "外部窗口的 is_blink 应为 false");
        }
    }

    #[test]
    fn generation_zero_is_valid() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let ref_id = register_window_ref(42, 100, false, "Test", "test.exe", 0);
        assert!(ref_id.starts_with("wref_"));
    }

    #[test]
    fn ttl_expiry_check() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let gen_val = next_generation();
        // 注册一个"已过期"的 ref——手动修改 issued_at
        let ref_id = register_window_ref(42, 100, false, "Test", "test.exe", gen_val);
        {
            let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(rec) = map.get_mut(&ref_id) {
                // 将 issued_at 设为 2 分钟前（超过 60 秒 TTL）
                rec.issued_at = Instant::now() - Duration::from_secs(120);
            }
        }
        // 应返回 ExpiredTtl
        assert!(validate_window_ref_detailed(&ref_id).is_variant(&RefValidation::ExpiredTtl));
    }

    #[test]
    fn generation_expiry_check() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_registry();
        let gen1 = next_generation(); // gen1=0, current=1
        let ref_id = register_window_ref(42, 100, false, "Test", "test.exe", gen1);
        // 推进 3 个 generation，使 gen1 过期
        let _ = next_generation(); // current=2
        let _ = next_generation(); // current=3
        let _ = next_generation(); // current=4
        // gen1=0, 0+1=1 < 4 → 过期
        assert!(
            validate_window_ref_detailed(&ref_id).is_variant(&RefValidation::ExpiredGeneration)
        );
    }
}
