use crate::model::{Adapter, Address, NetworkSnapshot, adapter_kind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use windows::Win32::{
    Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, ERROR_SUCCESS},
    NetworkManagement::{IpHelper::*, Ndis::IfOperStatusUp},
    Networking::WinSock::*,
};
use winreg::{RegKey, enums::HKEY_CURRENT_USER};

pub fn snapshot(enhanced: bool) -> NetworkSnapshot {
    let mut result = NetworkSnapshot {
        proxy: if enhanced { system_proxy() } else { None },
        ..Default::default()
    };
    // API nodes are 8-byte aligned and owned by buffer until copied into Rust values.
    let mut bytes = 15 * 1024u32;
    let mut flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    if enhanced {
        flags |= GAA_FLAG_INCLUDE_GATEWAYS;
    }
    for _ in 0..3 {
        if bytes > 1024 * 1024 {
            break;
        }
        let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
        let head = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        let status = unsafe {
            GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(head), &mut bytes)
        };
        if status == ERROR_BUFFER_OVERFLOW.0 {
            continue;
        }
        if status == ERROR_NO_DATA.0 {
            return result;
        }
        if status != ERROR_SUCCESS.0 {
            break;
        }
        // SAFETY: successful API call provides all linked nodes in the live buffer;
        // socket_ip checks sockaddr length and family before typed reads.
        unsafe {
            let mut node = head;
            while !node.is_null() {
                let adapter = &*node;
                node = adapter.Next;
                if adapter.OperStatus != IfOperStatusUp || adapter.IfType == 24 {
                    continue;
                }
                let name = adapter
                    .FriendlyName
                    .to_string()
                    .unwrap_or_else(|_| "网络适配器".into());
                let description = adapter.Description.to_string().unwrap_or_default();
                // Only metadata needed to recognize the physical adapter is read in
                // basic mode; don't traverse virtual/tunnel address lists.
                if !enhanced
                    && adapter_kind(&name, &description, adapter.IfType, &[])
                        != crate::model::AdapterKind::Physical
                {
                    continue;
                }
                let mut addresses = Vec::new();
                let mut address = adapter.FirstUnicastAddress;
                while !address.is_null() {
                    let row = &*address;
                    address = row.Next;
                    if row.DadState != IpDadStatePreferred && row.DadState != IpDadStateDeprecated {
                        continue;
                    }
                    if let Some(ip) = socket_ip(&row.Address) {
                        if ip.is_loopback() || ip.is_unspecified() {
                            continue;
                        }
                        if let Some(address) = Address::from_lifetimes(
                            ip,
                            row.OnLinkPrefixLength,
                            row.ValidLifetime,
                            if row.DadState == IpDadStatePreferred {
                                row.PreferredLifetime
                            } else {
                                0
                            },
                            temporary_ipv6(ip, row.SuffixOrigin),
                        ) {
                            addresses.push(address);
                        }
                    }
                }
                addresses.sort_by_key(|a| (a.ip, a.prefix));
                addresses.dedup();
                if addresses.is_empty() {
                    continue;
                }
                let mut gateways = Vec::new();
                let mut gateway = if enhanced {
                    adapter.FirstGatewayAddress
                } else {
                    std::ptr::null_mut()
                };
                while !gateway.is_null() {
                    let row = &*gateway;
                    gateway = row.Next;
                    if let Some(ip) = socket_ip(&row.Address) {
                        if !ip.is_unspecified() {
                            let scoped = if matches!(ip, IpAddr::V6(v) if v.is_unicast_link_local())
                            {
                                format!("{ip}%{}", adapter.Ipv6IfIndex)
                            } else {
                                ip.to_string()
                            };
                            if !gateways.contains(&scoped) {
                                gateways.push(scoped);
                            }
                        }
                    }
                }
                let kind = adapter_kind(&name, &description, adapter.IfType, &addresses);
                if !enhanced && kind != crate::model::AdapterKind::Physical {
                    continue;
                }
                result.adapters.push(Adapter {
                    name,
                    kind,
                    addresses,
                    gateways,
                    metric: adapter.Ipv4Metric,
                });
            }
        }
        result
            .adapters
            .sort_by_key(|a| (a.kind, a.gateways.is_empty(), a.metric, a.name.clone()));
        return result;
    }
    result.warning = Some("读取本机网卡信息失败，出口检测仍可使用".into());
    result
}

fn temporary_ipv6(ip: IpAddr, suffix: NL_SUFFIX_ORIGIN) -> bool {
    // Windows privacy IPv6 addresses use random suffixes. TRANSIENT marks
    // cluster addresses, not temporary privacy addresses.
    ip.is_ipv6() && suffix == IpSuffixOriginRandom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_ipv6_uses_suffix_origin_instead_of_cluster_transient_flag() {
        let ip = "2409:8a55:410:f510:3955:bbf7:9a98:a6eb".parse().unwrap();
        assert!(temporary_ipv6(ip, IpSuffixOriginRandom));
        for suffix in [
            IpSuffixOriginDhcp,
            IpSuffixOriginLinkLayerAddress,
            IpSuffixOriginManual,
        ] {
            assert!(!temporary_ipv6(ip, suffix));
        }
        assert!(!temporary_ipv6(
            "192.168.1.2".parse().unwrap(),
            IpSuffixOriginRandom
        ));
    }
}

unsafe fn socket_ip(address: &SOCKET_ADDRESS) -> Option<IpAddr> {
    if address.lpSockaddr.is_null()
        || address.iSockaddrLength < std::mem::size_of::<SOCKADDR>() as i32
    {
        return None;
    }
    // SAFETY: caller provides an API-owned sockaddr; size/family gate each cast.
    unsafe {
        match (*address.lpSockaddr).sa_family {
            AF_INET if address.iSockaddrLength >= std::mem::size_of::<SOCKADDR_IN>() as i32 => {
                let value = &*address.lpSockaddr.cast::<SOCKADDR_IN>();
                Some(IpAddr::V4(Ipv4Addr::from(
                    value.sin_addr.S_un.S_addr.to_ne_bytes(),
                )))
            }
            AF_INET6 if address.iSockaddrLength >= std::mem::size_of::<SOCKADDR_IN6>() as i32 => {
                let value = &*address.lpSockaddr.cast::<SOCKADDR_IN6>();
                Some(IpAddr::V6(Ipv6Addr::from(value.sin6_addr.u.Byte)))
            }
            _ => None,
        }
    }
}

fn system_proxy() -> Option<String> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
        .ok()?;
    if key.get_value::<u32, _>("ProxyEnable").unwrap_or(0) == 0 {
        return None;
    }
    let server = key.get_value::<String, _>("ProxyServer").ok()?;
    // Display/fingerprint host:port only, never retain userinfo or URL secrets.
    let safe: String = server
        .split(';')
        .map(|s| {
            s.rsplit('@')
                .next()
                .unwrap_or("")
                .split(['?', '#'])
                .next()
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("; ")
        .chars()
        .take(200)
        .collect();
    (!safe.is_empty()).then_some(safe)
}
