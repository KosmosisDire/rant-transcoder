//! Which peers run on this host. A peer is local when the IP of its announced address is one
//! of this host's.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};

/// This host's IPv4 addresses, loopback included.
pub fn host_addresses() -> HashSet<Ipv4Addr> {
    let mut out = HashSet::from([Ipv4Addr::LOCALHOST]);
    // with no interface list, this host's own nodes on loopback still count
    for interface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let std::net::IpAddr::V4(ip) = interface.ip() {
            out.insert(ip);
        }
    }
    out
}

/// The IP of a Rant peer's "ip:port" address.
pub fn address_ip(address: &str) -> Option<Ipv4Addr> {
    address.parse::<SocketAddrV4>().ok().map(|a| *a.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_peer_address() {
        assert_eq!(address_ip("192.168.1.20:7400"), Some(Ipv4Addr::new(192, 168, 1, 20)));
        assert_eq!(address_ip("192.168.1.20"), None);
    }

    #[test]
    fn this_host_includes_loopback() {
        assert!(host_addresses().contains(&Ipv4Addr::LOCALHOST));
    }
}
