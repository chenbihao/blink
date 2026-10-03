use super::{DiscoveredEntry, Discovery, LaunchTarget, parser};
use std::{collections::HashMap, path::PathBuf, time::UNIX_EPOCH};
use windows::{
    Win32::{
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
        UI::{
            Shell::{
                SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHLoadIndirectString,
                ShellExecuteExW,
            },
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
    core::PCWSTR,
};

fn system_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into()))
        .join("System32")
}
fn catalog_dir() -> PathBuf {
    system_dir()
        .parent()
        .unwrap()
        .join("ImmersiveControlPanel\\Settings")
}

fn files() -> Vec<PathBuf> {
    let Ok(files) = std::fs::read_dir(catalog_dir()) else {
        return Vec::new();
    };
    let mut files: Vec<_> = files
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("AllSystemSettings_") && s.ends_with(".xml"))
        })
        .take(16)
        .collect();
    files.sort();
    files
}
pub fn fingerprint() -> Vec<(String, u64, u64)> {
    files()
        .into_iter()
        .filter_map(|path| {
            let meta = path.metadata().ok()?;
            let time = meta
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos() as u64;
            Some((path.to_string_lossy().into_owned(), meta.len(), time))
        })
        .collect()
}
fn expand(value: &str) -> String {
    let root = system_dir()
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    value
        .replace("%windir%", &root)
        .replace("%WINDIR%", &root)
        .replace("%SystemRoot%", &root)
        .replace("%systemroot%", &root)
}
fn load_resource(value: &str) -> Option<String> {
    let value: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let mut buffer = vec![0u16; 8192];
    unsafe {
        SHLoadIndirectString(PCWSTR(value.as_ptr()), &mut buffer, None).ok()?;
    }
    let len = buffer.iter().position(|c| *c == 0)?;
    Some(String::from_utf16_lossy(&buffer[..len]))
}
fn resource(value: &str) -> Option<String> {
    if !value.starts_with('@') {
        return Some(value.to_string());
    }
    if let Some(text) = load_resource(&expand(value)) {
        return Some(text);
    }
    // Windows 的内部 windows 别名不是包全名；使用系统 PRI 文件作显式资源来源。
    let resource = value.strip_prefix("@{windows?")?.strip_suffix('}')?;
    let root = system_dir().parent()?.to_path_buf();
    for file in [
        root.join(
            "SystemResources/Windows.UI.SettingsAppThreshold/Windows.UI.SettingsAppThreshold.pri",
        ),
        root.join("ImmersiveControlPanel/resources.pri"),
    ] {
        if !file.is_file() {
            continue;
        }
        if let Some(text) = load_resource(&format!("@{{{}?{resource}}}", file.display())) {
            return Some(text);
        }
    }
    None
}
fn keywords(value: &str) -> Vec<String> {
    // 多个 DLL 引用可能以 @@ 连接；现代 @{...} 资源不能按分隔符拆开。
    let pieces = if value.trim().starts_with("@{") {
        value
            .split(';')
            .filter_map(|piece| resource(piece.trim()))
            .collect()
    } else if value.contains('@') {
        value
            .split('@')
            .filter(|s| !s.trim().is_empty())
            .filter_map(|s| resource(&format!("@{}", s.trim().trim_end_matches(';'))))
            .collect::<Vec<_>>()
    } else {
        vec![value.to_string()]
    };
    pieces
        .into_iter()
        .flat_map(|s| {
            s.split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}
fn settings_available(uri: &str) -> bool {
    use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
    let build = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()
        .and_then(|key| key.get_value::<String, _>("CurrentBuildNumber").ok())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let minimum = match uri {
        "ms-settings:clipboard" => 17763,
        "ms-settings:startupapps" | "ms-settings:sound" => 17134,
        _ => 10240,
    };
    if build < minimum {
        return false;
    }
    match uri {
        "ms-settings:network-wifi" => unsafe {
            use windows::Win32::{Foundation::HANDLE, NetworkManagement::WiFi::*};
            let mut handle = HANDLE::default();
            let mut version = 0;
            if WlanOpenHandle(2, None, &mut version, &mut handle) != 0 {
                return false;
            }
            let mut list = std::ptr::null_mut();
            let found = WlanEnumInterfaces(handle, None, &mut list) == 0
                && !list.is_null()
                && (*list).dwNumberOfItems > 0;
            if !list.is_null() {
                WlanFreeMemory(list.cast());
            }
            WlanCloseHandle(handle, None);
            found
        },
        "ms-settings:bluetooth" => unsafe {
            use windows::Win32::{
                Devices::Bluetooth::*,
                Foundation::{CloseHandle, HANDLE},
            };
            let mut radio = HANDLE::default();
            let parameters = BLUETOOTH_FIND_RADIO_PARAMS {
                dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
            };
            let Ok(search) = BluetoothFindFirstRadio(&parameters, &mut radio) else {
                return false;
            };
            let _ = CloseHandle(radio);
            let _ = BluetoothFindRadioClose(search);
            true
        },
        _ => true,
    }
}
pub fn available(target: &LaunchTarget) -> bool {
    match target {
        LaunchTarget::Program { file, .. } => system_dir().join(file).is_file(),
        LaunchTarget::Shell(_) => true,
        LaunchTarget::Uri(uri) => settings_available(uri),
    }
}
pub fn discover() -> Result<Option<Discovery>, String> {
    let started = std::time::Instant::now();
    let paths = files();
    if paths.is_empty() {
        return if catalog_dir().is_dir() {
            Err("无法读取 Windows 设置目录".into())
        } else {
            Ok(None)
        };
    }
    let mut result = Discovery::default();
    let mut availability = HashMap::new();
    for path in paths {
        let records = (|| {
            if path.metadata().map_err(|e| e.to_string())?.len() > 8 * 1024 * 1024 {
                return Err("设置目录文件过大".into());
            }
            use std::io::Read;
            let mut xml = String::new();
            std::fs::File::open(&path)
                .map_err(|e| e.to_string())?
                .take(8 * 1024 * 1024 + 1)
                .read_to_string(&mut xml)
                .map_err(|e| e.to_string())?;
            if xml.len() > 8 * 1024 * 1024 {
                return Err("设置目录文件过大".into());
            }
            parser::parse(&xml)
        })();
        let records = match records {
            Ok(rows) => rows,
            Err(error) => {
                result.failed_files += 1;
                tracing::warn!(path = %path.display(), %error, "系统设置目录读取失败");
                continue;
            }
        };
        for record in records {
            if record.conditional || record.id.is_empty() {
                result.skipped += 1;
                continue;
            }
            if !record.host.is_empty()
                && !matches!(
                    record.host.to_ascii_uppercase().as_str(),
                    "{12B1697E-D3A0-4DBC-B568-CCF64A3F934D}"
                        | "{7E0522FC-1AC4-41CA-AFD0-3610417A9C41}"
                )
            {
                result.skipped += 1;
                continue;
            }
            let Some(target) = parser::record_target(&record).filter(|target| match target {
                LaunchTarget::Uri(uri) => *availability
                    .entry(uri.clone())
                    .or_insert_with(|| available(target)),
                _ => available(target),
            }) else {
                result.skipped += 1;
                continue;
            };
            let Some(title) = resource(&record.title).filter(|s| !s.is_empty()) else {
                result.skipped += 1;
                continue;
            };
            // 保留同身份的标题/关键词变体，统一交由 domain 合并成一个入口。
            let icon = expand(record.icon.split(',').next().unwrap_or(""));
            result.entries.push(DiscoveredEntry {
                id: record.id.to_ascii_lowercase(),
                title,
                keywords: keywords(&record.keywords),
                icon,
                target,
            });
        }
    }
    if result.failed_files > 0 && result.entries.is_empty() {
        return Err("Windows 设置目录解析失败，请重试".into());
    }
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        entries = result.entries.len(),
        skipped = result.skipped,
        failed_files = result.failed_files,
        "系统设置目录解析完成"
    );
    Ok(Some(result))
}
pub fn launch(target: &LaunchTarget) -> Result<(), String> {
    if !available(target) {
        return Err("系统入口当前不可用".into());
    }
    let (file, parameters) = match target {
        LaunchTarget::Shell(path) | LaunchTarget::Uri(path) => (path.clone(), String::new()),
        LaunchTarget::Program { file, parameters } => (
            system_dir().join(file).to_string_lossy().into_owned(),
            parameters.clone(),
        ),
    };
    let file: Vec<u16> = file.encode_utf16().chain(Some(0)).collect();
    let parameters: Vec<u16> = parameters.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let result = ShellExecuteExW(&mut info).map_err(|e| e.to_string());
        if initialized {
            CoUninitialize();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "本机只读目录回放，不执行系统入口"]
    fn replay_installed_catalog() {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let discovery = super::discover().unwrap();
        if let Some(discovery) = discovery {
            assert!(!discovery.entries.is_empty());
            assert!(
                discovery
                    .entries
                    .iter()
                    .any(|e| e.title.contains("环境变量") || e.title.contains("environment"))
            );
        }
    }
}
