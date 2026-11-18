//! Address policy (Phase 7, §9): where a friends connection
//! may go.
//!
//! Privacy Mode ON (the default) changes NOTHING about sync,
//! approval, or handoff to paired peers on private addresses —
//! but public addresses are refused ABSOLUTELY, and DNS names
//! that resolve to a public address (DNS rebinding) are refused
//! too. The check runs on EVERY resolved address, not just the
//! first: one public address in the answer set refuses the
//! whole name.
//!
//! Accepted: loopback, RFC 1918 private, link-local, unique
//! local (ULA), and CGNAT 100.64.0.0/10 (Tailscale/tailnet
//! traffic rides CGNAT space). Everything else — including
//! documentation ranges and unspecified addresses — is refused.

use crate::error::MimintError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

/// Is this address on the accepted set (private / loopback /
/// link-local / ULA / CGNAT)?
pub fn is_accepted_address(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local() || is_cgnat(v4),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || is_unique_local(v6)
                || v6.is_unicast_link_local()
                || is_ipv4_mapped_accepted(v6)
        }
    }
}

/// 100.64.0.0/10 (RFC 6598 CGNAT / tailnet space).
pub fn is_cgnat(addr: Ipv4Addr) -> bool {
    let octets = addr.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

/// fc00::/7 unique local addresses.
pub fn is_unique_local(addr: Ipv6Addr) -> bool {
    (addr.segments()[0] & 0xfe00) == 0xfc00
}

/// IPv4-mapped IPv6 addresses defer to the embedded IPv4 rule.
fn is_ipv4_mapped_accepted(addr: Ipv6Addr) -> bool {
    match addr.to_ipv4_mapped() {
        Some(v4) => is_accepted_address(IpAddr::V4(v4)),
        None => false,
    }
}

/// Resolve a host name and check EVERY resolved address. The
/// returned addresses are all accepted, or an error refuses
/// the connection. A literal IP is checked directly (no DNS).
pub fn resolve_and_check_all(host: &str, port: u16) -> Result<Vec<IpAddr>, MimintError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_accepted_address(ip) {
            return Ok(vec![ip]);
        }
        return Err(MimintError::Config(format!(
            "refusing public address {host}: friends connections stay on private/tailnet addresses"
        )));
    }
    let resolved: Vec<IpAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|err| MimintError::Config(format!("cannot resolve {host}: {err}")))?
        .map(|addr| addr.ip())
        .collect();
    if resolved.is_empty() {
        return Err(MimintError::Config(format!(
            "no addresses resolved for {host}"
        )));
    }
    for addr in &resolved {
        if !is_accepted_address(*addr) {
            return Err(MimintError::Config(format!(
                "refusing {host}: it resolves to public address {addr} (possible DNS rebinding)"
            )));
        }
    }
    Ok(resolved)
}

/// Check one resolved address list (the pure core of the DNS
/// check, used by tests with injected answers).
pub fn check_resolved_addresses(addrs: &[IpAddr]) -> Result<(), MimintError> {
    if addrs.is_empty() {
        return Err(MimintError::Config("no addresses resolved".to_string()));
    }
    for addr in addrs {
        if !is_accepted_address(*addr) {
            return Err(MimintError::Config(format!(
                "refusing public address {addr} (possible DNS rebinding)"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(text: &str) -> IpAddr {
        IpAddr::V4(text.parse().unwrap())
    }

    #[test]
    fn accepted_set_covers_private_loopback_cgnat_and_ula() {
        assert!(is_accepted_address(v4("127.0.0.1")));
        assert!(is_accepted_address(v4("10.0.0.5")));
        assert!(is_accepted_address(v4("172.16.0.9")));
        assert!(is_accepted_address(v4("192.168.1.1")));
        assert!(is_accepted_address(v4("169.254.1.2")));
        assert!(is_accepted_address(v4("100.64.0.1")));
        assert!(is_accepted_address(v4("100.127.255.255")));
        assert!(is_accepted_address(IpAddr::V6("::1".parse().unwrap())));
        assert!(is_accepted_address(IpAddr::V6("fd00::1".parse().unwrap())));
        assert!(is_accepted_address(IpAddr::V6("fe80::1".parse().unwrap())));
    }

    #[test]
    fn public_addresses_are_refused() {
        assert!(!is_accepted_address(v4("8.8.8.8")));
        assert!(!is_accepted_address(v4("1.1.1.1")));
        assert!(!is_accepted_address(v4("100.128.0.1"))); // Past CGNAT.
        assert!(!is_accepted_address(v4("0.0.0.0")));
        assert!(!is_accepted_address(v4("203.0.113.5"))); // TEST-NET-3.
        assert!(!is_accepted_address(IpAddr::V6(
            "2001:db8::1".parse().unwrap()
        )));
    }

    #[test]
    fn dns_rebinding_one_public_address_refuses_all() {
        let mixed = vec![v4("192.168.1.5"), v4("8.8.8.8")];
        assert!(check_resolved_addresses(&mixed).is_err());
        let clean = vec![v4("192.168.1.5"), v4("10.0.0.2")];
        assert!(check_resolved_addresses(&clean).is_ok());
        assert!(check_resolved_addresses(&[]).is_err());
    }

    #[test]
    fn literal_public_ip_refused_without_dns() {
        assert!(resolve_and_check_all("8.8.8.8", 19120).is_err());
        assert!(resolve_and_check_all("127.0.0.1", 19120).is_ok());
    }
}
