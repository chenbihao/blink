//! Windows 平台特定的搜索实现：扫描开始菜单 .lnk 文件 + UWP/MSIX 应用。

use std::path::PathBuf;

use super::AppEntry;

/// 扫描 shell:AppsFolder 中的 UWP/MSIX 应用。
///
/// 使用 Windows Shell API 枚举 `shell:AppsFolder` 虚拟文件夹，获取所有已注册的
/// 打包应用（UWP/MSIX）。每个应用通过 AppUserModelId 标识，`lnk_path` 格式为
/// `shell:AppsFolder\{AppUserModelId}`，Windows Shell 原生支持此格式启动。
pub fn scan_apps_folder() -> Vec<AppEntry> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        BHID_StorageEnum, BHID_PropertyStore, IEnumShellItems, IShellItem,
        SHCreateItemFromParsingName, SIGDN_NORMALDISPLAY,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
    use windows::Win32::System::Variant::VT_LPWSTR;

    let start = std::time::Instant::now();
    let mut entries = Vec::new();

    // COM 初始化 RAII guard
    let com_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let should_uninit = com_hr.is_ok();
    struct ComUninit(bool);
    impl Drop for ComUninit {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }
    let _com = ComUninit(should_uninit);

    let result = (|| -> Result<(), windows::core::Error> {
        unsafe {
            // 打开 shell:AppsFolder
            let apps_folder: IShellItem =
                SHCreateItemFromParsingName(windows::core::w!("shell:AppsFolder"), None)?;

            // 枚举子项
            let enum_items: IEnumShellItems =
                apps_folder.BindToHandler(None, &BHID_StorageEnum)?;

            let mut fetched: u32 = 0;
            let mut item_array: [Option<IShellItem>; 1] = [None];

            while enum_items.Next(&mut item_array, Some(&mut fetched)).is_ok() && fetched > 0 {
                let Some(ref item) = item_array[0] else {
                    continue;
                };

                // 获取显示名
                let display_name = match item.GetDisplayName(SIGDN_NORMALDISPLAY) {
                    Ok(pwstr) => {
                        let name = pwstr.to_string().unwrap_or_default();
                        // 释放 PWSTR（CoTaskMemAlloc 分配的）
                        if !pwstr.is_null() {
                            windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.as_ptr() as *const _));
                        }
                        name
                    }
                    Err(_) => continue,
                };

                if display_name.is_empty() {
                    continue;
                }

                // 获取 AppUserModelId
                let app_user_model_id = match item.BindToHandler::<_, IPropertyStore>(None, &BHID_PropertyStore) {
                    Ok(prop_store) => {
                        let prop_var = prop_store.GetValue(&PKEY_AppUserModel_ID);
                        match prop_var {
                            Ok(var) if var.Anonymous.Anonymous.vt == VT_LPWSTR => {
                                let pwstr = var.Anonymous.Anonymous.Anonymous.pwszVal;
                                let id = if !pwstr.is_null() {
                                    pwstr.to_string().unwrap_or_default()
                                } else {
                                    String::new()
                                };
                                // PropVariantClear 释放内部资源
                                let mut var_mut = var;
                                let _ = windows::Win32::System::Com::StructuredStorage::PropVariantClear(&mut var_mut);
                                id
                            }
                            Ok(mut var) => {
                                let _ = windows::Win32::System::Com::StructuredStorage::PropVariantClear(&mut var);
                                String::new()
                            }
                            Err(_) => String::new(),
                        }
                    }
                    Err(_) => String::new(),
                };

                if app_user_model_id.is_empty() {
                    continue;
                }

                let lnk_path = format!("shell:AppsFolder\\{}", app_user_model_id);
                let pinyin_name = super::to_pinyin_initials(&display_name);

                entries.push(AppEntry {
                    name: display_name,
                    pinyin_name,
                    description: Some(lnk_path.clone()),
                    lnk_path,
                    is_calc: false,
                    score: 0.0,
                    is_placeholder: false,
                    is_error: false,
                    source: "start_menu".to_string(),
                    action: super::Action {
                        kind: super::ActionKind::Open,
                        hint: None,
                        payload: None,
                    },
                    score_detail: None,
                });
            }
        }
        Ok(())
    })();

    if let Err(e) = result {
        tracing::debug!(error = %e, "scan_apps_folder 失败");
    }

    let elapsed = start.elapsed();
    tracing::debug!(count = entries.len(), elapsed_ms = elapsed.as_millis(), "AppsFolder 扫描完成");

    entries
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
                    is_error: false,
                    source: "start_menu".to_string(),
                    action: super::Action {
                        kind: super::ActionKind::Open,
                        hint: None,
                        payload: None,
                    },
                    score_detail: None,
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
    Some(AppEntry {
        name,
        pinyin_name,
        description: Some(lnk_path.to_string()),
        lnk_path: lnk_path.to_string(),
        is_calc: false,
        score: 0.0,
        is_placeholder: false,
        is_error: false,
        source: "start_menu".to_string(),
        action: super::Action {
            kind: super::ActionKind::Open,
            hint: None,
            payload: None,
        },
        score_detail: None,
    })
}
