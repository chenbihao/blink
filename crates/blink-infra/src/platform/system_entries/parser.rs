use super::{LaunchTarget, settings_uri};
use quick_xml::{Reader, events::Event};

#[derive(Default, Debug)]
pub(super) struct Record {
    pub id: String,
    pub title: String,
    pub keywords: String,
    pub icon: String,
    pub link: String,
    pub page: String,
    pub host: String,
    pub conditional: bool,
}

pub(super) fn parse(xml: &str) -> Result<Vec<Record>, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = true;
    let mut records = Vec::new();
    let mut current = None::<Record>;
    let mut tag = Vec::new();
    let mut page_node = false;
    let mut depth = 0usize;
    loop {
        match reader.read_event().map_err(|e| format!("设置 XML: {e}"))? {
            Event::Start(e) => {
                depth += 1;
                tag = e.name().as_ref().to_vec();
                if tag == b"SearchableContent" {
                    let mut record = Record::default();
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| e.to_string())?;
                        if matches!(
                            attr.key.as_ref(),
                            b"IncludeWithFeature" | b"ExcludeWithFeature"
                        ) {
                            record.conditional = true;
                        }
                    }
                    current = Some(record);
                }
                if let Some(record) = &mut current {
                    if tag == b"Condition"
                        || e.attributes().flatten().any(|a| {
                            matches!(
                                a.key.as_ref(),
                                b"IncludeWithFeature" | b"ExcludeWithFeature"
                            )
                        })
                    {
                        record.conditional = true;
                    }
                }
                page_node = tag == b"Node"
                    && e.attributes()
                        .flatten()
                        .any(|a| a.key.as_ref() == b"Type" && a.value.as_ref() == b"Page");
            }
            Event::Text(e) => {
                let text = e.decode().map_err(|e| e.to_string())?;
                if let Some(record) = &mut current {
                    append(record, &tag, page_node, &text);
                }
            }
            Event::GeneralRef(e) => {
                let name = e.decode().map_err(|e| e.to_string())?;
                let escaped = format!("&{name};");
                let text = quick_xml::escape::unescape(&escaped).map_err(|e| e.to_string())?;
                if let Some(record) = &mut current {
                    append(record, &tag, page_node, &text);
                }
            }
            Event::End(e) => {
                depth = depth.checked_sub(1).ok_or("设置 XML 层级无效")?;
                if matches!(e.name().as_ref(), b"Keywords" | b"HighKeywords")
                    && let Some(record) = &mut current
                {
                    record.keywords.push(';');
                }
                if e.name().as_ref() == b"SearchableContent"
                    && let Some(record) = current.take()
                {
                    let mut record = record;
                    for field in [
                        &mut record.id,
                        &mut record.title,
                        &mut record.keywords,
                        &mut record.icon,
                        &mut record.link,
                        &mut record.page,
                        &mut record.host,
                    ] {
                        *field = field.trim().to_string();
                    }
                    records.push(record);
                }
                tag.clear();
                page_node = false;
            }
            Event::DocType(_) => return Err("设置 XML 不支持 DTD".into()),
            Event::Eof => break,
            _ => (),
        }
    }
    if current.is_some() || depth != 0 {
        return Err("设置 XML 未完整结束".into());
    }
    Ok(records)
}

fn append(record: &mut Record, tag: &[u8], page_node: bool, text: &str) {
    match tag {
        b"Filename" => {
            if record.id.trim().is_empty() {
                record.id.push_str(text);
            }
        }
        b"SettingID" => record.id = text.into(),
        b"Description" => record.title.push_str(text),
        b"Keywords" | b"HighKeywords" => record.keywords.push_str(text),
        b"Icon" => record.icon.push_str(text),
        b"DeepLink" => record.link.push_str(text),
        b"PageID" if text.trim().starts_with("SettingsPage") => set_page(record, text),
        b"Node" if page_node => set_page(record, text),
        b"HostID" => record.host.push_str(text),
        b"Condition" => record.conditional = true,
        _ => (),
    }
}

fn set_page(record: &mut Record, text: &str) {
    let value = text.trim();
    if record.page.is_empty() || record.page == value {
        record.page = value.into();
    } else {
        record.conditional = true;
    } // 多个不同页面需要版本/条件判断，首版跳过。
}
fn canonical(name: &str) -> Option<LaunchTarget> {
    if !matches!(
        name,
        "Microsoft.System"
            | "Microsoft.DeviceManager"
            | "Microsoft.WindowsFirewall"
            | "Microsoft.IndexingOptions"
            | "Microsoft.FolderOptions"
            | "Microsoft.PowerOptions"
            | "Microsoft.UserAccounts"
            | "Microsoft.NetworkAndSharingCenter"
            | "Microsoft.CredentialManager"
            | "Microsoft.Keyboard"
            | "Microsoft.Mouse"
            | "Microsoft.Sound"
            | "Microsoft.InternetOptions"
            | "Microsoft.Fonts"
            | "Microsoft.ColorManagement"
            | "Microsoft.DateAndTime"
            | "Microsoft.RegionAndLanguage"
            | "Microsoft.AutoPlay"
            | "Microsoft.Recovery"
            | "Microsoft.FileHistory"
            | "Microsoft.SyncCenter"
            | "Microsoft.StorageSpaces"
            | "Microsoft.DefaultPrograms"
            | "Microsoft.EaseOfAccessCenter"
    ) {
        return None;
    }
    Some(LaunchTarget::Program {
        file: "control.exe".into(),
        parameters: format!("/name {name}"),
    })
}

/// 受支持的系统命令集合；未知命令不进入可执行目录。
pub(super) fn target(link: &str, page: &str) -> Option<LaunchTarget> {
    let link = link.trim();
    if link.is_empty() {
        return settings_uri(page).map(|uri| LaunchTarget::Uri(uri.into()));
    }
    if link.starts_with("ms-settings:") {
        return super::supported_uri(link).then(|| LaunchTarget::Uri(link.into()));
    }
    if link.starts_with("shell:") {
        return matches!(
            link,
            "shell:RecycleBinFolder"
                | "shell:ControlPanelFolder"
                | "shell:MyComputerFolder"
                | "shell:NetworkFolder"
                | "shell:::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}"
        )
        .then(|| LaunchTarget::Shell(link.into()));
    }
    if link.starts_with("Microsoft.") {
        return canonical(link);
    }
    let (file, parameters) = if let Some(quoted) = link.strip_prefix('"') {
        let (file, rest) = quoted.split_once('"')?;
        (file, rest.trim())
    } else {
        link.split_once(' ')
            .map_or((link, ""), |(file, rest)| (file, rest.trim()))
    };
    let file = file.replace('/', "\\");
    let bare = file.rsplit('\\').next()?.to_ascii_lowercase();
    // An absolute source must refer to System32; never substitute a different binary silently.
    if file.contains('\\')
        && !file
            .to_ascii_lowercase()
            .ends_with(&format!("\\system32\\{bare}"))
    {
        return None;
    }
    let accepted = match bare.as_str() {
        "rundll32.exe" => matches!(
            parameters.to_ascii_lowercase().as_str(),
            "sysdm.cpl,editenvironmentvariables"
        ),
        "control.exe" => parameters
            .strip_prefix("/name ")
            .and_then(canonical)
            .is_some(),
        "systempropertiesadvanced.exe"
        | "systempropertiescomputername.exe"
        | "systempropertieshardware.exe"
        | "systempropertiesperformance.exe"
        | "systempropertiesprotection.exe"
        | "systempropertiesremote.exe"
        | "taskmgr.exe"
        | "msinfo32.exe"
        | "optionalfeatures.exe"
        | "odbcad32.exe"
        | "appwiz.cpl"
        | "sysdm.cpl"
        | "ncpa.cpl"
        | "mmsys.cpl"
        | "powercfg.cpl"
        | "timedate.cpl"
        | "inetcpl.cpl"
        | "main.cpl"
        | "firewall.cpl"
        | "devmgmt.msc"
        | "services.msc"
        | "diskmgmt.msc"
        | "compmgmt.msc"
        | "eventvwr.msc"
        | "taskschd.msc"
        | "certmgr.msc" => parameters.is_empty(),
        _ => false,
    };
    accepted.then(|| LaunchTarget::Program {
        file: bare,
        parameters: parameters.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_identity_keywords_and_conditions() {
        let rows = parse(r#"<PCSettings><SearchableContent><Filename>x</Filename><SettingIdentity><SettingID>id</SettingID></SettingIdentity><SettingInformation><Description>A &amp; B</Description><HighKeywords>one;</HighKeywords><Keywords>two;</Keywords></SettingInformation></SearchableContent><SearchableContent IncludeWithFeature="x"><Filename>hidden</Filename></SearchableContent></PCSettings>"#).unwrap();
        assert_eq!(rows[0].id, "id");
        assert_eq!(rows[0].title, "A & B");
        assert!(rows[0].keywords.contains("two"));
        assert!(rows[1].conditional);
        assert!(parse("<PCSettings><SearchableContent>").is_err());
    }
    #[test]
    fn modern_pages_and_unknown_conditions_are_not_guessed() {
        let records = parse(r#"<PCSettings><SearchableContent><Filename>modern-id</Filename><SettingIdentity><PageID>SettingsPageAudio</PageID><SettingPaths><Path><Node Type="Page">SettingsPageAudio</Node></Path></SettingPaths></SettingIdentity><SettingInformation><Description>声音</Description><HighKeywords>扬声器</HighKeywords><Keywords>音量</Keywords></SettingInformation></SearchableContent><SearchableContent><Filename>ambiguous</Filename><SettingPaths><Node Type="Page">SettingsPageAudio</Node><Node Type="Page">SettingsPagePCSystemDisplay</Node></SettingPaths></SearchableContent><SearchableContent><Filename>conditional</Filename><Condition/></SearchableContent></PCSettings>"#).unwrap();
        assert_eq!(
            target("", &records[0].page),
            Some(LaunchTarget::Uri("ms-settings:sound".into()))
        );
        assert_eq!(records[0].keywords, "扬声器;音量;");
        assert!(records[1].conditional && records[2].conditional);
        assert!(parse("<PCSettings><SearchableContent></SearchableContent>").is_err());
        assert!(parse("<!DOCTYPE x><PCSettings/>").is_err());
    }
    #[test]
    fn targets_are_typed_and_unknown_commands_are_rejected() {
        assert_eq!(
            target(
                "%windir%\\system32\\rundll32.exe sysdm.cpl,EditEnvironmentVariables",
                ""
            ),
            Some(LaunchTarget::Program {
                file: "rundll32.exe".into(),
                parameters: "sysdm.cpl,EditEnvironmentVariables".into()
            })
        );
        assert!(target("cmd.exe /c echo hello", "").is_none());
        assert!(target("C:\\other\\taskmgr.exe", "").is_none());
        assert!(target("rundll32.exe evil.dll,Entry", "").is_none());
        assert!(target("", "UnknownPage").is_none());
        assert!(target("Microsoft.DoesNotExist", "").is_none());
        assert!(target("ms-settings:unknown-page", "").is_none());
        assert!(target("shell:AppsFolder/unknown", "").is_none());
        assert!(matches!(
            target("Microsoft.System", ""),
            Some(LaunchTarget::Program { .. })
        ));
        assert!(matches!(
            target("", "SettingsPageDisplay"),
            Some(LaunchTarget::Uri(_))
        ));
    }
}
