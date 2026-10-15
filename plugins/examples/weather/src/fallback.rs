//! Explicit, opt-in provider fallback. The service owns the original deadline and cancellation.
use crate::protocol::{PluginError, PluginToCore};
use crate::settings::{self, RequestParameters};
use serde_json::{Value, json};
use std::collections::VecDeque;

pub struct Plan {
    pub next: VecDeque<RequestParameters>,
    pub requested: String,
    pub active: String,
    pub failures: Vec<Value>,
}

pub fn retryable(error: &PluginError) -> bool {
    matches!(
        error.code.as_str(),
        "provider_not_configured"
            | "authentication_failed"
            | "network_failed"
            | "invalid_response"
            | "rate_limited"
            | "quota_exceeded"
            | "timeout"
    )
}

impl Plan {
    pub fn record(&mut self, error: &PluginError) {
        self.failures
            .push(json!({"provider":self.active,"code":error.code,"message":error.message}));
    }
}

pub fn prepare(
    query: Option<&str>,
    args: Option<&Value>,
    config: Option<&Value>,
) -> Result<(RequestParameters, Plan), PluginError> {
    let invalid = || PluginError {
        code: "invalid_arguments".into(),
        message: "天气降级配置格式无效".into(),
    };
    let enabled = match config.and_then(|v| v.get("allow_fallback")) {
        None => false,
        Some(value) => value.as_bool().ok_or_else(invalid)?,
    };
    let requested = config
        .and_then(|v| v.get("default_provider"))
        .and_then(Value::as_str)
        .unwrap_or("open_meteo")
        .to_owned();
    let mut plan = Plan {
        next: VecDeque::new(),
        requested: requested.clone(),
        active: requested.clone(),
        failures: vec![],
    };
    let primary = settings::parse(query, args, config);
    if !enabled {
        return primary.map(|parameters| (parameters, plan));
    }
    let order = match config.and_then(|v| v.get("fallback_order")) {
        None => vec!["open_meteo", "qweather", "caiyun"],
        Some(value) => value
            .as_array()
            .ok_or_else(invalid)?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| matches!(*value, "open_meteo" | "qweather" | "caiyun"))
                    .ok_or_else(invalid)
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    if order.len() > 3 {
        return Err(invalid());
    }
    let mut seen = vec![requested.as_str()];
    for provider in order {
        if seen.contains(&provider) {
            continue;
        }
        seen.push(provider);
        let mut config = config.cloned().unwrap_or_else(|| json!({}));
        config["default_provider"] = json!(provider);
        // A broken preferred geocoder must not block the next independent provider.
        if provider != "qweather" {
            config.as_object_mut().unwrap().remove("qweather_api_host");
            config.as_object_mut().unwrap().remove("qweather_api_key");
        }
        if let Ok(parameters) = settings::parse(query, args, Some(&config)) {
            plan.next.push_back(parameters);
        }
    }
    match primary {
        Ok(parameters) => Ok((parameters, plan)),
        Err(error) if retryable(&error) && !plan.next.is_empty() => {
            plan.record(&error);
            let parameters = plan.next.pop_front().unwrap();
            plan.active = super::service::provider_name(parameters.provider).into();
            Ok((parameters, plan))
        }
        Err(error) => Err(error),
    }
}

/// Preserve data status and actual source, while exposing the reason in every output.
pub fn annotate(message: &mut PluginToCore, plan: &Plan) {
    if plan.failures.is_empty() {
        return;
    }
    let reasons = plan
        .failures
        .iter()
        .map(|failure| {
            format!(
                "{}：{}",
                crate::presentation::provider_label(failure["provider"].as_str().unwrap_or("")),
                failure["message"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("；");
    let note = format!(
        "已降级至 {}（{}）",
        crate::presentation::provider_label(&plan.active),
        reasons
    );
    let decorate = |data: &mut Value| {
        data["fallback"] = json!({"requested_provider":plan.requested,"actual_provider":plan.active,"attempts":plan.failures});
        if let Some(text) = data.get_mut("copy_text") {
            *text = json!(format!("{}\n{note}", text.as_str().unwrap_or("")));
        }
    };
    match message {
        PluginToCore::RawResult(result) => {
            if let Some(data) = result.data.get_mut(0) {
                decorate(data);
            }
        }
        PluginToCore::Response(result) => {
            for item in &mut result.items {
                if let Some(payload) = &mut item.payload {
                    decorate(payload);
                }
                if let crate::protocol::PluginAction::Copy { text } = &mut item.action {
                    text.push_str(&format!("\n{note}"));
                    item.subtitle = Some(format!(
                        "{} · {note}",
                        item.subtitle.as_deref().unwrap_or("")
                    ));
                }
            }
        }
        _ => {}
    }
}
