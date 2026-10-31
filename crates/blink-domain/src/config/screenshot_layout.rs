use serde::{Deserialize, Serialize};

pub const TOOLBAR_ACTIONS: &[&str] = &[
    "translate",
    "scroll",
    "gif",
    "save",
    "ask-ai",
    "pin",
    "ocr",
    "qr",
    "translate-pin",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawLayout")]
pub struct ScreenshotToolbarLayout {
    pub toolbar: Vec<String>,
    pub more: Vec<String>,
}
#[derive(Deserialize, Default)]
struct RawLayout {
    #[serde(default)]
    toolbar: Vec<String>,
    #[serde(default)]
    more: Vec<String>,
}
impl Default for ScreenshotToolbarLayout {
    fn default() -> Self {
        Self {
            toolbar: TOOLBAR_ACTIONS[..6].iter().map(|id| (*id).into()).collect(),
            more: TOOLBAR_ACTIONS[6..].iter().map(|id| (*id).into()).collect(),
        }
    }
}
impl From<RawLayout> for ScreenshotToolbarLayout {
    fn from(raw: RawLayout) -> Self {
        let mut seen = std::collections::HashSet::new();
        let mut clean = |ids: Vec<String>| {
            ids.into_iter()
                .filter(|id| TOOLBAR_ACTIONS.contains(&id.as_str()) && seen.insert(id.clone()))
                .collect::<Vec<_>>()
        };
        let toolbar = clean(raw.toolbar);
        let mut more = clean(raw.more);
        more.extend(
            TOOLBAR_ACTIONS
                .iter()
                .filter(|id| !seen.contains(**id))
                .map(|id| (*id).to_string()),
        );
        Self { toolbar, more }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_field_is_default_but_custom_layout_appends_omissions_to_more() {
        let old: crate::config::ScreenshotConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(old.toolbar_layout, ScreenshotToolbarLayout::default());
        let layout: ScreenshotToolbarLayout = serde_json::from_value(
            serde_json::json!({"toolbar":["pin","pin","unknown"],"more":["qr","pin"]}),
        )
        .unwrap();
        assert_eq!(layout.toolbar, ["pin"]);
        assert_eq!(layout.more[0], "qr");
        assert_eq!(
            layout.toolbar.len() + layout.more.len(),
            TOOLBAR_ACTIONS.len()
        );
        assert_eq!(
            serde_json::from_value::<ScreenshotToolbarLayout>(
                serde_json::to_value(&layout).unwrap()
            )
            .unwrap(),
            layout
        );
    }
}
