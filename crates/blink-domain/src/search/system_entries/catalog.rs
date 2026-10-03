use blink_infra::platform::system_entries::LaunchTarget;

pub struct Definition {
    pub id: &'static str,
    pub zh: &'static str,
    pub en: &'static str,
    pub link: &'static str,
    pub aliases: &'static str,
}
macro_rules! entry {
    ($id:literal, $zh:literal, $en:literal, $link:literal, $aliases:literal) => {
        Definition {
            id: $id,
            zh: $zh,
            en: $en,
            link: $link,
            aliases: $aliases,
        }
    };
}
pub const DEFINITIONS: &[Definition] = &[
    entry!(
        "recycle_bin",
        "回收站",
        "Recycle Bin",
        "shell:RecycleBinFolder",
        "垃圾桶;recycle"
    ),
    entry!(
        "control_panel",
        "控制面板",
        "Control Panel",
        "shell:ControlPanelFolder",
        "打开控制面板;control"
    ),
    entry!(
        "environment_variables",
        "账户环境变量",
        "Account Environment Variables",
        "rundll32.exe sysdm.cpl,EditEnvironmentVariables",
        "环境变量;编辑账户的环境变量;用户环境变量;env;envvar"
    ),
    entry!(
        "this_pc",
        "此电脑",
        "This PC",
        "shell:MyComputerFolder",
        "我的电脑;计算机;computer"
    ),
    entry!(
        "network",
        "网络",
        "Network",
        "shell:NetworkFolder",
        "网络邻居;network folder"
    ),
    entry!(
        "programs_features",
        "程序和功能",
        "Programs and Features",
        "appwiz.cpl",
        "卸载程序;程序卸载;uninstall programs"
    ),
    entry!(
        "system_properties",
        "系统属性",
        "System Properties",
        "sysdm.cpl",
        "系统属性设置"
    ),
    entry!(
        "device_manager",
        "设备管理器",
        "Device Manager",
        "devmgmt.msc",
        "设备管理;驱动管理;device"
    ),
    entry!(
        "task_manager",
        "任务管理器",
        "Task Manager",
        "taskmgr.exe",
        "任务管理;taskmgr"
    ),
    entry!(
        "services",
        "服务",
        "Services",
        "services.msc",
        "系统服务;服务管理;services"
    ),
    entry!(
        "system_environment",
        "系统环境变量",
        "System Environment Variables",
        "SystemPropertiesAdvanced.exe",
        "编辑系统环境变量;打开系统环境变量;系统变量;system env;path"
    ),
    entry!(
        "advanced_system",
        "高级系统设置",
        "Advanced System Settings",
        "SystemPropertiesAdvanced.exe",
        "查看高级系统设置;高级系统属性;advanced system"
    ),
    entry!(
        "network_connections",
        "网络连接",
        "Network Connections",
        "ncpa.cpl",
        "网卡;网络适配器;adapter;ethernet"
    ),
    entry!(
        "disk_management",
        "磁盘管理",
        "Disk Management",
        "diskmgmt.msc",
        "分区;disk;partition"
    ),
    entry!(
        "computer_management",
        "计算机管理",
        "Computer Management",
        "compmgmt.msc",
        "电脑管理"
    ),
    entry!(
        "event_viewer",
        "事件查看器",
        "Event Viewer",
        "eventvwr.msc",
        "事件日志;event log"
    ),
    entry!(
        "task_scheduler",
        "任务计划程序",
        "Task Scheduler",
        "taskschd.msc",
        "计划任务;scheduled tasks"
    ),
    entry!(
        "display",
        "显示设置",
        "Display Settings",
        "ms-settings:display",
        "显示;分辨率;缩放;屏幕;display;resolution"
    ),
    entry!(
        "sound",
        "声音设置",
        "Sound Settings",
        "ms-settings:sound",
        "声音;音频;扬声器;麦克风;sound;audio"
    ),
    entry!(
        "bluetooth",
        "蓝牙设置",
        "Bluetooth Settings",
        "ms-settings:bluetooth",
        "蓝牙;bluetooth"
    ),
    entry!(
        "mouse",
        "鼠标设置",
        "Mouse Settings",
        "ms-settings:mousetouchpad",
        "鼠标;mouse"
    ),
    entry!(
        "printers",
        "打印机和扫描仪",
        "Printers and Scanners",
        "ms-settings:printers",
        "打印机;扫描仪;printer"
    ),
    entry!(
        "power",
        "电源设置",
        "Power Settings",
        "ms-settings:powersleep",
        "电源;睡眠;power;sleep"
    ),
    entry!(
        "storage",
        "存储设置",
        "Storage Settings",
        "ms-settings:storagesense",
        "存储;磁盘空间;storage"
    ),
    entry!(
        "proxy",
        "代理设置",
        "Proxy Settings",
        "ms-settings:network-proxy",
        "代理;网络代理;proxy"
    ),
    entry!(
        "vpn",
        "VPN 设置",
        "VPN Settings",
        "ms-settings:network-vpn",
        "vpn;虚拟专用网络"
    ),
    entry!(
        "wifi",
        "Wi-Fi 设置",
        "Wi-Fi Settings",
        "ms-settings:network-wifi",
        "wifi;无线网络;无线;wi-fi"
    ),
    entry!(
        "default_apps",
        "默认应用",
        "Default Apps",
        "ms-settings:defaultapps",
        "默认程序;文件关联;default apps"
    ),
    entry!(
        "startup_apps",
        "启动应用",
        "Startup Apps",
        "ms-settings:startupapps",
        "启动项;开机启动;startup"
    ),
    entry!(
        "date_time",
        "日期和时间",
        "Date and Time",
        "ms-settings:dateandtime",
        "时间;日期;时钟;date;time"
    ),
    entry!(
        "language",
        "语言设置",
        "Language Settings",
        "ms-settings:regionlanguage",
        "语言;输入语言;language"
    ),
    entry!(
        "clipboard",
        "系统剪贴板设置",
        "Windows Clipboard Settings",
        "ms-settings:clipboard",
        "剪贴板设置;clipboard settings"
    ),
    entry!(
        "windows_update",
        "Windows 更新",
        "Windows Update",
        "ms-settings:windowsupdate",
        "系统更新;检查更新;windows update"
    ),
    entry!(
        "recovery",
        "恢复设置",
        "Recovery Settings",
        "ms-settings:recovery",
        "恢复;重置电脑;recovery;reset pc"
    ),
    entry!(
        "about",
        "关于此电脑",
        "About This PC",
        "ms-settings:about",
        "系统信息;系统版本;about pc"
    ),
    entry!(
        "optional_features",
        "Windows 功能",
        "Windows Features",
        "optionalfeatures.exe",
        "启用或关闭Windows功能;可选功能;windows features"
    ),
    entry!(
        "firewall",
        "Windows 防火墙",
        "Windows Firewall",
        "firewall.cpl",
        "防火墙;firewall"
    ),
];
impl Definition {
    pub fn target(&self) -> LaunchTarget {
        if self.link.starts_with("shell:") {
            LaunchTarget::Shell(self.link.into())
        } else if self.link.starts_with("ms-settings:") {
            LaunchTarget::Uri(self.link.into())
        } else {
            let (file, parameters) = self.link.split_once(' ').unwrap_or((self.link, ""));
            LaunchTarget::Program {
                file: file.into(),
                parameters: parameters.into(),
            }
        }
    }
    pub fn history_key(&self) -> String {
        if DEFINITIONS.iter().take(10).any(|d| d.id == self.id) {
            self.link.into()
        } else {
            format!("system:{}", self.id)
        }
    }
    pub fn icon(&self) -> String {
        match self.target() {
            LaunchTarget::Shell(path) => path,
            LaunchTarget::Program { file, .. } => file,
            LaunchTarget::Uri(_) => "stock:30".into(),
        }
    }
    pub fn matches_discovery(&self, id: &str, title: &str, target: &LaunchTarget) -> bool {
        let known = match id {
            "{37092408-d49c-451d-b56d-78b243dc475c}" => Some("environment_variables"),
            "{e2394c16-f45a-496f-83cc-49e163281662}" => Some("system_environment"),
            "{b1fe5142-dedd-409b-bcc8-547ec08de84e}" => Some("advanced_system"),
            _ => None,
        };
        known == Some(self.id)
            || (self.target() == *target
                && (self.zh == title
                    || self.en == title
                    || self.aliases.split(';').any(|a| a == title)))
    }
}
