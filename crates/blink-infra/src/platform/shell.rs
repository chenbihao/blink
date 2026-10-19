//! Windows Shell API——UWP/MSIX 应用枚举（0.14.6 §2.3 从 domain/search/windows.rs 迁入）。
//!
//! 把 Win32 Shell API 调用（COM init / SHCreateItemFromParsingName / IPropertyStore）
//! 收进 infra，返回原始数据 `(display_name, shell_path)`。domain 层用这些数据
//! 构造 `AppEntry`，不直接 `use windows::`。

/// 扫描 shell:AppsFolder 中的 UWP/MSIX 应用，返回 `(display_name, shell_path)` 列表。
///
/// `shell_path` 格式为 `shell:AppsFolder\{AppUserModelId}`，Windows Shell 原生支持此格式启动。
#[cfg(target_os = "windows")]
pub fn scan_uwp_apps() -> Vec<(String, String)> {
    use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{
        BHID_PropertyStore, BHID_StorageEnum, IEnumShellItems, IShellItem,
        SHCreateItemFromParsingName, SIGDN_NORMALDISPLAY,
    };

    let start = std::time::Instant::now();
    let mut results = Vec::new();

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
            let enum_items: IEnumShellItems = apps_folder.BindToHandler(None, &BHID_StorageEnum)?;

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
                            windows::Win32::System::Com::CoTaskMemFree(Some(
                                pwstr.as_ptr() as *const _
                            ));
                        }
                        name
                    }
                    Err(_) => continue,
                };

                if display_name.is_empty() {
                    continue;
                }

                // 获取 AppUserModelId
                let app_user_model_id = match item
                    .BindToHandler::<_, IPropertyStore>(None, &BHID_PropertyStore)
                {
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

                let shell_path = format!("shell:AppsFolder\\{}", app_user_model_id);
                results.push((display_name, shell_path));
            }
        }
        Ok(())
    })();

    if let Err(e) = result {
        tracing::debug!(error = %e, "scan_uwp_apps 失败");
    }

    let elapsed = start.elapsed();
    tracing::debug!(
        count = results.len(),
        elapsed_ms = elapsed.as_millis(),
        "AppsFolder 扫描完成"
    );

    results
}

#[cfg(not(target_os = "windows"))]
pub fn scan_uwp_apps() -> Vec<(String, String)> {
    Vec::new()
}

/// 以管理员权限运行指定程序（UAC 提权，0.26.14）。
///
/// ShellExecuteW verb="runas"：弹 UAC 确认框，用户同意后以提升权限启动；
/// 拒绝时返回 `Cancelled`。用于 Everything 服务的显式安装
/// （`everything.exe -instance blink -install-service`）——**只在用户显式
/// 点击设置页按钮时调用**，绝不在后台静默提权。
///
/// 返回 `Ok(())` 仅表示提权请求已受理（ShellExecuteAsync 语义），安装是否
/// 成功由用户侧 UAC 与服务安装结果决定，调用方不据此断言服务已装好。
#[cfg(target_os = "windows")]
pub fn run_elevated(exe: &std::path::Path, args: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe_w: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let args_w: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();
    let verb_w: Vec<u16> = "runas\0".encode_utf16().collect();

    // ShellExecuteW 返回值 > 32 表示成功（Win32 惯例）。
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb_w.as_ptr()),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as usize > 32 {
        Ok(())
    } else {
        // SE_ERR_ACCESSDENIED(5) 之外的常见失败：用户取消 UAC 通常表现为 5。
        Err(format!("ShellExecuteW runas 失败（code={}）", result.0 as i32))
    }
}

#[cfg(not(target_os = "windows"))]
pub fn run_elevated(_exe: &std::path::Path, _args: &str) -> Result<(), String> {
    Err("run_elevated 仅支持 Windows".to_string())
}
