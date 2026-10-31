//! QR-specific projection of the shared URL/path probes. Never probes a network path.
use super::probe::{is_file_path, is_url};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QrTarget {
    Text,
    Web { target: String, host: String },
    Directory { target: String },
    File { target: String },
    Unavailable,
}

fn local_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    is_file_path(path)
        && bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
        && !path
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '<' | '>' | '|' | '*' | '?'))
        && !path[2..].contains(':')
}

fn valid_percent_encoding(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().all(|(i, byte)| {
        *byte != b'%'
            || bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    })
}

/// Pure URI adaptation; the caller alone may check local metadata in a blocking task.
pub fn parse_target(text: &str) -> QrTarget {
    if text.len() > 32_767 || text.chars().any(char::is_control) {
        return QrTarget::Text;
    }
    let text = text.trim();
    if is_url(text) {
        if let Ok(url) = url::Url::parse(text) {
            match url.scheme() {
                "http" | "https" if !text.contains('\\') => {
                    if let Some(host) = url.host_str() {
                        return QrTarget::Web {
                            target: url.to_string(),
                            host: host.to_string(),
                        };
                    }
                }
                "file"
                    if text
                        .split_once("://")
                        .is_some_and(|(_, rest)| rest.starts_with('/'))
                        && valid_percent_encoding(text)
                        && url.host_str().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none() =>
                {
                    // Url handles UTF-8 / percent decoding / drive letters; no prefix stripping.
                    if let Ok(path) = url.to_file_path() {
                        let path = path.to_string_lossy().into_owned();
                        if local_drive_path(&path) {
                            return QrTarget::File { target: path };
                        }
                    }
                }
                _ => {}
            }
        }
    } else if local_drive_path(text) {
        return QrTarget::File {
            target: text.to_string(),
        };
    }
    QrTarget::Text
}

/// Resolves symlinks before metadata: links to UNC/device paths must not trigger network IO.
/// Drive roots are additionally checked by infra so mapped network drives are excluded.
pub fn classify(text: &str) -> QrTarget {
    let parsed = parse_target(text);
    let QrTarget::File { target } = parsed else {
        return parsed;
    };
    match blink_infra::platform::fs_guard::probe_local_target(&target) {
        Ok(true) => QrTarget::Directory { target },
        Ok(false) => QrTarget::File { target },
        Err(_) => QrTarget::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_qr_whitelist_preserves_shared_probe_contract() {
        for text in [
            "ftp://example.com",
            "javascript:alert(1)",
            "https://",
            "example.com",
            "<a>https://example.com</a>",
            r"\\server\share",
            "file://server/share/a",
            "file://localhost/C:/a",
            r"\\?\C:\a",
            "C:relative",
            "file:///C:/a%00b",
            r"C:\a:stream",
        ] {
            assert_eq!(parse_target(text), QrTarget::Text, "{text}");
        }
        assert!(is_url("ftp://example.com"));
        assert!(
            matches!(parse_target("https://user@example.com/path?q=x"), QrTarget::Web { host, .. } if host == "example.com")
        );
        assert!(matches!(
            parse_target(r"C:\中文\a % b.txt"),
            QrTarget::File { .. }
        ));
    }
    #[test]
    #[cfg(windows)]
    fn file_uri_decodes_once_without_losing_original_percent() {
        assert_eq!(
            parse_target("file:///C:/%E4%B8%AD%E6%96%87/a%20%25.txt"),
            QrTarget::File {
                target: "C:\\中文\\a %.txt".into()
            }
        );
    }

    #[test]
    #[cfg(windows)]
    fn local_metadata_classification_uses_isolated_unicode_space_and_percent_paths() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("中文 space %");
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join("a %20.txt");
        std::fs::write(&file, b"fixture").unwrap();
        assert!(matches!(
            classify(directory.to_str().unwrap()),
            QrTarget::Directory { .. }
        ));
        assert!(matches!(
            classify(file.to_str().unwrap()),
            QrTarget::File { .. }
        ));
        let uri = url::Url::from_file_path(&file).unwrap();
        assert_eq!(classify(uri.as_str()), classify(file.to_str().unwrap()));
        assert_eq!(
            classify(directory.join("missing.txt").to_str().unwrap()),
            QrTarget::Unavailable
        );
    }
}
