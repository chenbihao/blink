//! Windows 平台特定的搜索实现：扫描开始菜单 .lnk 文件 + UWP/MSIX 应用。
//!
//! 0.14.6 §2.3：Win32 Shell API 调用已迁至 `infra/platform/shell.rs`，
//! 本文件只负责用 infra 返回的原始数据构造 `AppEntry`。

use std::path::PathBuf;

use super::AppEntry;

/// 扫描 shell:AppsFolder 中的 UWP/MSIX 应用。
///
/// 0.14.6 §2.3：Win32 Shell API 调用委托到 `infra::platform::shell::scan_uwp_apps`，
/// 本函数只负责把原始 `(display_name, shell_path)` 数据构造成 `AppEntry`。
pub fn scan_apps_folder() -> Vec<AppEntry> {
    let raw = crate::infra::platform::shell::scan_uwp_apps();
    raw.into_iter()
        .map(|(display_name, lnk_path)| {
            let pinyin_name = super::to_pinyin_initials(&display_name);
            let pinyin_full = super::to_pinyin_full(&display_name);
            AppEntry {
                name: display_name,
                pinyin_name,
                pinyin_full,
                description: Some(lnk_path.clone()),
                lnk_path,
                is_calc: false,
                score: 0.0,
                is_placeholder: false,
                is_error: false,
                source: "start_menu".to_string(),
                actions: vec![super::Action::default()],
                ..Default::default()
            }
        })
        .collect()
}

/// 扫描用户 + 系统开始菜单，收集所有 .lnk 条目。
/// `max_depth` 控制递归深度（1=只扫根目录，2=扫一层子目录…）。
pub fn scan_start_menu(max_depth: u32) -> Vec<AppEntry> {
    let mut entries = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        scan_dir(
            &PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs"),
            &mut entries,
            max_depth,
            0,
        );
    }
    if let Ok(program_data) = std::env::var("ProgramData") {
        scan_dir(
            &PathBuf::from(program_data).join("Microsoft\\Windows\\Start Menu\\Programs"),
            &mut entries,
            max_depth,
            0,
        );
    }
    entries
}

fn scan_dir(dir: &PathBuf, entries: &mut Vec<AppEntry>, max_depth: u32, current_depth: u32) {
    if current_depth >= max_depth {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, entries, max_depth, current_depth + 1);
        } else if path.extension().is_some_and(|ext| ext == "lnk") {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if !name.is_empty() {
                let pinyin_name = super::to_pinyin_initials(&name);
                let pinyin_full = super::to_pinyin_full(&name);
                let lnk_path = path.to_string_lossy().to_string();
                entries.push(AppEntry {
                    name,
                    pinyin_name,
                    pinyin_full,
                    description: Some(lnk_path.clone()), // 副行显示路径
                    lnk_path,
                    is_calc: false,
                    score: 0.0,
                    is_placeholder: false,
                    is_error: false,
                    source: "start_menu".to_string(),
                    actions: vec![super::Action::default()],
                    ..Default::default()
                });
            }
        }
    }
}

/// 启动应用：直接打开 lnk 文件，Windows 自动解析并启动对应 exe。
pub fn launch(lnk_path: &str) -> Result<(), String> {
    open::that(lnk_path).map_err(|e| e.to_string())
}

/// 开始菜单两个根目录(用户 / 系统)的修改时间，用于缓存失效检测。
pub fn roots_modified() -> Vec<Option<std::time::SystemTime>> {
    let roots = start_menu_roots();
    roots
        .into_iter()
        .map(|p| std::fs::metadata(&p).ok().and_then(|m| m.modified().ok()))
        .collect()
}

/// 获取开始菜单根目录列表。
pub fn start_menu_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        roots.push(PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    if let Ok(program_data) = std::env::var("ProgramData") {
        roots.push(PathBuf::from(program_data).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    roots
}

/// 解析单个 .lnk 文件为 AppEntry（用于增量扫描）。
pub fn parse_lnk_entry(lnk_path: &str) -> Option<AppEntry> {
    let path = PathBuf::from(lnk_path);
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    if name.is_empty() {
        return None;
    }

    let pinyin_name = super::to_pinyin_initials(&name);
    let pinyin_full = super::to_pinyin_full(&name);
    Some(AppEntry {
        name,
        pinyin_name,
        pinyin_full,
        description: Some(lnk_path.to_string()),
        lnk_path: lnk_path.to_string(),
        is_calc: false,
        score: 0.0,
        is_placeholder: false,
        is_error: false,
        source: "start_menu".to_string(),
        actions: vec![super::Action::default()],
        ..Default::default()
    })
}
