use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Address {
    pub ip: IpAddr,
    pub prefix: u8,
    pub valid: bool,
    pub preferred: bool,
    pub temporary: bool,
}

impl Address {
    // Store lifecycle states rather than ticking lifetimes so a healthy snapshot
    // remains a stable cache fingerprint between queries.
    pub fn from_lifetimes(
        ip: IpAddr,
        prefix: u8,
        valid: u32,
        preferred: u32,
        temporary: bool,
    ) -> Option<Self> {
        (valid > 0).then_some(Self {
            ip,
            prefix,
            valid: true,
            preferred: preferred > 0,
            temporary,
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Physical,
    Tunnel,
    Virtual,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Adapter {
    pub name: String,
    pub kind: AdapterKind,
    pub addresses: Vec<Address>,
    pub gateways: Vec<String>,
    pub metric: u32,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct NetworkSnapshot {
    pub adapters: Vec<Adapter>,
    pub proxy: Option<String>,
    pub warning: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Domestic,
    International,
    Ipv6,
}

impl Group {
    pub fn id(self) -> &'static str {
        match self {
            Self::Domestic => "domestic",
            Self::International => "international",
            Self::Ipv6 => "ipv6",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Domestic => "国内站出口",
            Self::International => "国际站出口",
            Self::Ipv6 => "IPv6 出口",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub ip: IpAddr,
    pub provider: crate::providers::Provider,
    pub location: Option<String>,
    pub observed_at: String,
    pub fallback: bool,
    pub cached: bool,
}

/// 当前公网单播范围；拒绝 ULA、链路本地、映射 IPv4、文档与保留段。
pub fn public_v6(ip: Ipv6Addr) -> bool {
    let [a, b, ..] = ip.segments();
    (a & 0xe000) == 0x2000
        && !(a == 0x2001 && b < 0x0200)
        && !(a == 0x2001 && b == 0x0db8)
        && a != 0x2002
        && !(a == 0x3fff && (b & 0xf000) == 0)
}

/// 接口回包必须是公网 IPv4，不能把登录页、私网或 FAKE-IP 当成功。
pub fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && matches!(c, 0 | 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && matches!(b, 18 | 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

pub fn adapter_kind(
    name: &str,
    description: &str,
    if_type: u32,
    addresses: &[Address],
) -> AdapterKind {
    let text = format!("{name} {description}").to_lowercase();
    if matches!(if_type, 23 | 131) || ["mihomo", "wireguard", "wintun", "tunnel", "tun adapter", "tap-windows"]
        .iter().any(|s| text.contains(s))
        || addresses.iter().any(|a| matches!(a.ip, IpAddr::V4(ip) if ip.octets()[0] == 198 && matches!(ip.octets()[1], 18 | 19))) {
        AdapterKind::Tunnel
    } else if ["vethernet", "hyper-v", "vmware", "virtualbox", "virtual", "docker", "wsl", "tailscale", "zerotier"]
        .iter().any(|s| text.contains(s)) {
        AdapterKind::Virtual
    } else { AdapterKind::Physical }
}
