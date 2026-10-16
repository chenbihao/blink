use crate::{model::*, protocol::*, settings::Settings};
use serde_json::{Value, json};

pub fn response(
    id: String,
    tool: bool,
    settings: &Settings,
    network: &NetworkSnapshot,
    observations: &[(Group, Observation)],
    warnings: &[String],
) -> PluginToCore {
    let mut items = Vec::new();
    let mut raw = Vec::new();
    let preferred_v6 = |temporary| {
        network
            .adapters
            .iter()
            .filter(|a| a.kind == AdapterKind::Physical)
            .flat_map(|a| a.addresses.iter().map(move |ip| (a, ip)))
            .filter(|(_, a)| {
                a.valid
                    && a.preferred
                    && a.temporary == temporary
                    && matches!(a.ip, std::net::IpAddr::V6(v) if !v.is_unicast_link_local())
            })
            .min_by_key(|(adapter, a)| {
                (
                    matches!(a.ip, std::net::IpAddr::V6(v) if !public_v6(v)),
                    adapter.metric,
                    a.ip,
                )
            })
            .map(|(adapter, a)| (&adapter.name, a.ip))
    };
    let primary_v6 = preferred_v6(false);
    let temporary_v6 = preferred_v6(true);
    for adapter in &network.adapters {
        if !settings.enhanced && adapter.kind != AdapterKind::Physical {
            continue;
        }
        let (label, score) = match adapter.kind {
            AdapterKind::Physical => ("本机", 1.0),
            AdapterKind::Tunnel => ("隧道", 0.70),
            AdapterKind::Virtual => ("虚拟网卡", 0.65),
        };
        for address in &adapter.addresses {
            if !address.valid {
                continue;
            }
            let primary = primary_v6 == Some((&adapter.name, address.ip));
            let temporary = temporary_v6 == Some((&adapter.name, address.ip));
            if address.ip.is_ipv6()
                && (!settings.ipv6
                    || matches!(address.ip, std::net::IpAddr::V6(v) if v.is_unicast_link_local())
                    || (!settings.enhanced && !primary && !temporary))
            {
                continue;
            }
            let ip = address.ip.to_string();
            let family = if address.ip.is_ipv4() { "IPv4" } else { "IPv6" };
            let kind = if address.ip.is_ipv6() && !address.preferred {
                format!(
                    "{label} {family}（{}已弃用）",
                    if address.temporary { "临时，" } else { "" }
                )
            } else if address.ip.is_ipv6() && address.temporary {
                format!(
                    "{label} {family}（临时{}）",
                    if temporary { "" } else { "，其他" }
                )
            } else if address.ip.is_ipv6() && adapter.kind == AdapterKind::Physical && !primary {
                format!("{label} {family}（其他）")
            } else {
                format!("{label} {family}")
            };
            let mut detail = format!("{} · /{}", adapter.name, address.prefix);
            if address.ip.is_ipv6() {
                if !address.preferred {
                    detail.push_str(" · 已弃用，不建议使用");
                } else if address.temporary {
                    detail.push_str(" · 临时地址");
                } else {
                    detail.push_str(" · 非临时地址");
                }
                detail.push_str(" · 本机已分配，外网可达性见出口检测");
            }
            detail.push_str(" · 按 Enter 复制");
            add(
                &mut items,
                &mut raw,
                &ip,
                &kind,
                detail,
                if address.ip.is_ipv6() {
                    if primary {
                        0.85
                    } else if temporary {
                        0.84
                    } else {
                        0.60
                    }
                } else {
                    score
                },
                json!({"kind":"interface", "interface":adapter.name, "adapter_kind":adapter.kind, "prefix":address.prefix, "preferred":address.preferred, "temporary":address.temporary}),
            );
        }
        if settings.enhanced && adapter.kind == AdapterKind::Physical {
            for gateway in &adapter.gateways {
                if gateway.contains(':') && !settings.ipv6 {
                    continue;
                }
                add(
                    &mut items,
                    &mut raw,
                    gateway,
                    "网关",
                    format!("{} · 按 Enter 复制", adapter.name),
                    if gateway.contains(':') { 0.71 } else { 0.72 },
                    json!({"kind":"gateway", "interface":adapter.name}),
                );
            }
        }
    }
    for (index, (group, observation)) in observations.iter().enumerate() {
        if observations[..index]
            .iter()
            .any(|(_, previous)| previous.ip == observation.ip)
        {
            continue;
        }
        let peers: Vec<_> = observations
            .iter()
            .filter(|(_, o)| o.ip == observation.ip)
            .collect();
        let label = if peers.len() > 1 {
            if observation.ip.is_ipv6() {
                "IPv6 出口"
            } else {
                "国内/国际站出口"
            }
        } else {
            group.label()
        };
        let sources = peers
            .iter()
            .map(|(_, o)| o.provider.name())
            .collect::<Vec<_>>()
            .join(" / ");
        let mut detail = Vec::new();
        let location = peers.iter().find_map(|(_, o)| o.location.as_ref());
        if settings.show_location {
            if let Some(location) = location {
                detail.push(location.clone());
            }
        }
        detail.push(format!("来源：{sources}"));
        if peers.iter().any(|(_, o)| o.fallback) {
            detail.push("已使用备用源".into());
        }
        if let Some((_, cached)) = peers.iter().find(|(_, o)| o.cached) {
            detail.push(
                cached_time_at(&cached.observed_at, chrono::Local::now().fixed_offset())
                    .map(|time| format!("缓存于 {time}"))
                    .unwrap_or_else(|| "缓存".into()),
            );
        }
        detail.push("按 Enter 复制".into());
        add(
            &mut items,
            &mut raw,
            &observation.ip.to_string(),
            label,
            detail.join(" · "),
            if observation.ip.is_ipv6() { 0.80 } else { 0.90 },
            json!({"kind":"public", "sources":peers.iter().map(|(g,o)| json!({"scope":g.id(), "provider":o.provider.id(), "observed_at":o.observed_at, "cached":o.cached, "fallback":o.fallback})).collect::<Vec<_>>(), "location":location}),
        );
    }
    // 查询和工具保持相同顺序：本机 IPv4、出口 IPv4、常用 IPv6、出口 IPv6、增强详情。
    let mut rows: Vec<_> = items.into_iter().zip(raw).collect();
    rows.sort_by(|a, b| b.0.score.total_cmp(&a.0.score));
    let (mut items, mut raw): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
    let mut notes = warnings.to_vec();
    if let Some(warning) = &network.warning {
        notes.push(warning.clone());
    }
    if raw.is_empty() && notes.is_empty() {
        notes.push("未找到可用地址".into());
    }
    let error_notes = notes.len();
    if settings.enhanced {
        if let Some(proxy) = &network.proxy {
            notes.push(format!(
                "系统代理已启用：{proxy}；出口随目标站点和分流规则变化"
            ));
        } else if network
            .adapters
            .iter()
            .any(|a| a.kind == AdapterKind::Tunnel)
        {
            notes.push("检测到隧道网卡；出口随目标站点和分流规则变化".into());
        }
    }
    if let Some(first) = raw.first_mut() {
        first["warnings"] = json!(notes);
        first["status"] = json!(if warnings.is_empty() && network.warning.is_none() {
            "ok"
        } else {
            "partial"
        });
    }
    if tool {
        PluginToCore::RawResult {
            id,
            data: json!(raw),
            error: if raw.is_empty() {
                Some(PluginError {
                    code: "network_unavailable",
                    message: if notes.is_empty() {
                        "未找到可用地址".into()
                    } else {
                        notes.join("；")
                    },
                })
            } else {
                None
            },
        }
    } else {
        for (index, note) in notes.into_iter().enumerate() {
            let is_error = index < error_notes;
            items.push(PluginItem {
                title: note,
                subtitle: if is_error {
                    "网络提示 · 悬停查看全文，右键复制".into()
                } else {
                    "网络状态".into()
                },
                score: if is_error { -0.1 } else { 0.1 },
                action: PluginAction::None,
            });
        }
        PluginToCore::Response {
            id,
            items,
            error: None,
        }
    }
}

fn add(
    items: &mut Vec<PluginItem>,
    raw: &mut Vec<Value>,
    ip: &str,
    kind: &str,
    subtitle: String,
    score: f32,
    mut data: Value,
) {
    data["ip"] = json!(ip);
    data["type"] = json!(kind);
    raw.push(data);
    items.push(PluginItem {
        title: format!("{kind}：{ip}"),
        subtitle,
        score,
        action: PluginAction::Copy { text: ip.into() },
    });
}

fn cached_time_at(observed_at: &str, now: chrono::DateTime<chrono::FixedOffset>) -> Option<String> {
    let observed = chrono::DateTime::parse_from_rfc3339(observed_at)
        .ok()?
        .with_timezone(now.offset());
    let format = if observed.date_naive() == now.date_naive() {
        "%H:%M"
    } else {
        "%Y-%m-%d %H:%M"
    };
    Some(observed.format(format).to_string())
}

#[cfg(test)]
mod tests {
    use super::cached_time_at;
    #[test]
    fn cache_time_uses_24_hour_clock_and_user_timezone_without_offset_suffix() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-16T15:00:00+08:00").unwrap();
        assert_eq!(
            cached_time_at("2026-10-16T06:21:00Z", now).as_deref(),
            Some("14:21")
        );
        assert_eq!(
            cached_time_at("2026-10-15T23:59:00+08:00", now).as_deref(),
            Some("2026-10-15 23:59")
        );
        assert_eq!(cached_time_at("not a date", now), None);
    }
}
