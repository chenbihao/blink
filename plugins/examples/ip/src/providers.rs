use crate::model::{Observation, public_v4, public_v6};
use serde_json::Value;
use std::net::IpAddr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Ipip,
    Pubyun,
    Oray,
    Ipify,
    Ifconfig,
    IpifyV6,
    TestIpv6,
}

impl Provider {
    pub fn id(self) -> &'static str {
        match self {
            Self::Ipip => "ipip",
            Self::Pubyun => "pubyun",
            Self::Oray => "oray",
            Self::Ipify => "ipify",
            Self::Ifconfig => "ifconfig",
            Self::IpifyV6 => "ipify_v6",
            Self::TestIpv6 => "test_ipv6",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Ipip => "IPIP",
            Self::Pubyun => "公云 3322",
            Self::Oray => "花生壳",
            Self::Ipify | Self::IpifyV6 => "ipify",
            Self::Ifconfig => "ifconfig.co",
            Self::TestIpv6 => "test-ipv6.com",
        }
    }
    pub fn url(self) -> &'static str {
        match self {
            Self::Ipip => "https://myip.ipip.net/json",
            Self::Pubyun => "https://ip.3322.net/",
            Self::Oray => "https://ddns.oray.com/checkip",
            Self::Ipify => "https://api.ipify.org?format=json",
            Self::Ifconfig => "https://ifconfig.co/json",
            Self::IpifyV6 => "https://api6.ipify.org?format=json",
            Self::TestIpv6 => "https://ipv6.lookup.test-ipv6.com/ip/?callback=?",
        }
    }
    pub fn min_interval_ms(self) -> u64 {
        // 公云和 ifconfig.co 官方要求至少一分钟；其他源也限制主动刷新频率。
        match self {
            // Public diagnostic servers are also kept to one request per minute.
            Self::Pubyun | Self::Ifconfig | Self::TestIpv6 => 60_000,
            _ => 5_000,
        }
    }
    pub fn parse(self, body: &str) -> Result<Observation, &'static str> {
        if body.len() > 8192 {
            return Err("响应过大");
        }
        let payload = if self == Self::TestIpv6 {
            // JSONP is data only: accept the requested callback wrapper and parse
            // its single JSON argument; never evaluate scripts or arbitrary callbacks.
            let text = body.trim();
            text.strip_suffix(';')
                .unwrap_or(text)
                .strip_prefix("callback(")
                .and_then(|s| s.strip_suffix(')'))
                .ok_or("响应格式不正确")?
        } else {
            body
        };
        let value: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        let (text, location) = match self {
            Self::Ipip => {
                if value["ret"] != "ok" {
                    return Err("检测服务返回失败");
                }
                let location = value["data"]["location"].as_array().map(|rows| {
                    rows.iter()
                        .filter_map(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .take(6)
                        .map(clean)
                        .collect::<Vec<_>>()
                        .join(" · ")
                });
                (value["data"]["ip"].as_str().unwrap_or(""), location)
            }
            Self::Ipify | Self::IpifyV6 => (value["ip"].as_str().unwrap_or(""), None),
            Self::TestIpv6 => {
                if value["type"] != "ipv6" {
                    return Err("返回地址类型不匹配");
                }
                (value["ip"].as_str().unwrap_or(""), None)
            }
            Self::Ifconfig => {
                let parts: Vec<_> = ["country", "region_name", "city"]
                    .iter()
                    .filter_map(|k| value[k].as_str())
                    .filter(|s| !s.is_empty())
                    .map(clean)
                    .collect();
                (value["ip"].as_str().unwrap_or(""), Some(parts.join(" · ")))
            }
            Self::Pubyun => (body.trim(), None),
            Self::Oray => {
                let content = body.trim();
                let tail = if let Some(tail) = content.strip_prefix("Current IP Address:") {
                    tail
                } else if content.starts_with("<html>") {
                    content
                        .split_once("Current IP Address:")
                        .map(|(_, tail)| tail)
                        .ok_or("响应格式不正确")?
                } else {
                    return Err("响应格式不正确");
                };
                (
                    tail.trim()
                        .split(['<', '\r', '\n'])
                        .next()
                        .unwrap_or("")
                        .trim(),
                    None,
                )
            }
        };
        let ipv6 = matches!(self, Self::IpifyV6 | Self::TestIpv6);
        let ip: IpAddr = text.parse().map_err(|_| "没有返回有效 IP 地址")?;
        if ip.is_ipv6() != ipv6 {
            return Err("返回地址类型不匹配");
        }
        if !match ip {
            IpAddr::V4(v) => public_v4(v),
            IpAddr::V6(v) => public_v6(v),
        } {
            return Err("返回了非公网地址");
        }
        Ok(Observation {
            ip,
            provider: self,
            location: location.filter(|s| !s.is_empty()),
            observed_at: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            fallback: false,
            cached: false,
        })
    }
}

fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(70).collect()
}
