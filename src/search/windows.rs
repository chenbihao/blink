//! Windows 平台特定的搜索实现：扫描开始菜单 .lnk 文件。

use std::path::PathBuf;

use super::AppEntry;

/// 扫描用户 + 系统开始菜单，收集所有 .lnk 条目。
pub fn scan_start_menu() -> Vec<AppEntry> {
    let mut entries = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        scan_dir(
            &PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs"),
            &mut entries,
        );
    }
    if let Ok(program_data) = std::env::var("ProgramData") {
        scan_dir(
            &PathBuf::from(program_data).join("Microsoft\\Windows\\Start Menu\\Programs"),
            &mut entries,
        );
    }
    entries
}

fn scan_dir(dir: &PathBuf, entries: &mut Vec<AppEntry>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, entries);
        } else if path.extension().map_or(false, |ext| ext == "lnk") {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if !name.is_empty() {
                let pinyin_name = super::to_pinyin_initials(&name);
                let lnk_path = path.to_string_lossy().to_string();
                entries.push(AppEntry {
                    name,
                    pinyin_name,
                    description: Some(lnk_path.clone()), // 副行显示路径
                    lnk_path,
                    is_calc: false,
                    score: 0.0,
                    is_placeholder: false,
                    source: "start_menu".to_string(),
                    action: super::Action {
                        kind: super::ActionKind::Open,
                        hint: None,
                        payload: None,
                    },
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
    let mut roots = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        roots.push(PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    if let Ok(program_data) = std::env::var("ProgramData") {
        roots.push(PathBuf::from(program_data).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    roots
        .into_iter()
        .map(|p| std::fs::metadata(&p).ok().and_then(|m| m.modified().ok()))
        .collect()
}
