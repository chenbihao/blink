//! Windows 系统设置目录与类型化启动。读取和启动由调用方放入 blocking 线程。
mod parser;
#[cfg(windows)]
mod windows;

use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchTarget {
    Shell(String),
    Uri(String),
    Program { file: String, parameters: String },
}

#[derive(Clone, Debug)]
pub struct DiscoveredEntry {
    pub id: String,
    pub title: String,
    pub keywords: Vec<String>,
    pub icon: String,
    pub target: LaunchTarget,
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub entries: Vec<DiscoveredEntry>,
    pub skipped: usize,
    pub failed_files: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryState {
    #[default]
    Disabled,
    Building,
    Ready,
    Unsupported,
    Failed,
}

#[cfg(windows)]
pub use windows::{available, discover, fingerprint, launch};

#[cfg(not(windows))]
pub fn available(_target: &LaunchTarget) -> bool {
    false
}
#[cfg(not(windows))]
pub fn discover() -> Result<Option<Discovery>, String> {
    Ok(None)
}
#[cfg(not(windows))]
pub fn fingerprint() -> Vec<(String, u64, u64)> {
    Vec::new()
}
#[cfg(not(windows))]
pub fn launch(_target: &LaunchTarget) -> Result<(), String> {
    Err("当前平台不支持系统入口".into())
}

/// 明确维护的 PageID → URI 映射，不从任意标识猜测协议。
pub fn settings_uri(page: &str) -> Option<&'static str> {
    Some(match page {
        "SettingsPageDisplay" | "SettingsPagePCSystemDisplay" => "ms-settings:display",
        "SettingsPageSound" | "SettingsPageAudio" => "ms-settings:sound",
        "SettingsPageBluetooth" | "SettingsPagePCSystemBluetooth" => "ms-settings:bluetooth",
        "SettingsPageMouse" => "ms-settings:mousetouchpad",
        "SettingsPagePrinters" | "SettingsPageDevicesPrinters" => "ms-settings:printers",
        "SettingsPageNetworkProxy" => "ms-settings:network-proxy",
        "SettingsPageNetworkVPN" => "ms-settings:network-vpn",
        "SettingsPageNetworkWiFi" => "ms-settings:network-wifi",
        "SettingsPageAppsDefaults" => "ms-settings:defaultapps",
        "SettingsPageAppsStartup" | "SettingsPageStartup" => "ms-settings:startupapps",
        "SettingsPageDateTime" | "SettingsPageTimeRegionDateTime" => "ms-settings:dateandtime",
        "SettingsPageLanguage" | "SettingsPageTimeRegionLanguage" => "ms-settings:regionlanguage",
        "SettingsPageClipboard" => "ms-settings:clipboard",
        "SettingsPageWindowsUpdate" | "SettingsPageRestoreMusUpdate" => "ms-settings:windowsupdate",
        "SettingsPageRecovery" | "SettingsPageRestoreRestore" => "ms-settings:recovery",
        "SettingsPageStorage" | "SettingsPageStorageSenseStorageOverview" => {
            "ms-settings:storagesense"
        }
        "SettingsPageAbout" => "ms-settings:about",
        "SettingsPagePrivacyMicrophone" => "ms-settings:privacy-microphone",
        "SettingsPagePrivacyWebcam" => "ms-settings:privacy-webcam",
        "SettingsPagePrivacyLocation" => "ms-settings:privacy-location",
        "SettingsPageAppsNotifications" => "ms-settings:notifications",
        "SettingsPageInstalledApps" => "ms-settings:appsfeatures",
        "SettingsPageBackground" => "ms-settings:personalization-background",
        "SettingsPageColors" => "ms-settings:personalization-colors",
        "SettingsPageLockScreen" => "ms-settings:lockscreen",
        "SettingsPageStart" | "SettingsPageStart2" => "ms-settings:personalization-start",
        "SettingsPageTaskbar" => "ms-settings:taskbar",
        "SettingsPageThemes" => "ms-settings:themes",
        "SettingsPageNetworkEthernet" => "ms-settings:network-ethernet",
        "SettingsPageNetworkAirplaneMode" => "ms-settings:network-airplanemode",
        "SettingsPageActivate" => "ms-settings:activation",
        "SettingsPageEaseOfAccessMagnifier" => "ms-settings:easeofaccess-magnifier",
        "SettingsPageEaseOfAccessNarrator" => "ms-settings:easeofaccess-narrator",
        _ => return None,
    })
}

/// 直接 URI 也只接受维护的稳定页面，不导入任意协议/查询参数。
pub(super) fn supported_uri(uri: &str) -> bool {
    matches!(
        uri,
        "ms-settings:display"
            | "ms-settings:sound"
            | "ms-settings:bluetooth"
            | "ms-settings:mousetouchpad"
            | "ms-settings:printers"
            | "ms-settings:powersleep"
            | "ms-settings:network-proxy"
            | "ms-settings:network-vpn"
            | "ms-settings:network-wifi"
            | "ms-settings:defaultapps"
            | "ms-settings:startupapps"
            | "ms-settings:dateandtime"
            | "ms-settings:regionlanguage"
            | "ms-settings:clipboard"
            | "ms-settings:windowsupdate"
            | "ms-settings:recovery"
            | "ms-settings:storagesense"
            | "ms-settings:disksandvolumes"
            | "ms-settings:about"
            | "ms-settings:privacy-microphone"
            | "ms-settings:privacy-webcam"
            | "ms-settings:privacy-location"
            | "ms-settings:notifications"
            | "ms-settings:appsfeatures"
            | "ms-settings:personalization-background"
            | "ms-settings:personalization-colors"
            | "ms-settings:lockscreen"
            | "ms-settings:personalization-start"
            | "ms-settings:taskbar"
            | "ms-settings:themes"
            | "ms-settings:network-ethernet"
            | "ms-settings:network-airplanemode"
            | "ms-settings:activation"
            | "ms-settings:easeofaccess-magnifier"
            | "ms-settings:easeofaccess-narrator"
    )
}
