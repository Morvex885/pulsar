//! Choice of the LAN interface for multicast (mDNS) and broadcast (wake arbitration).

use std::net::Ipv4Addr;

use crate::sys;

pub struct Iface {
    pub name: String,
    pub index: u32,
    pub addr: Ipv4Addr,
}

/// The best usable IPv4 interface: up, multicast-capable, not loopback, not 169.254/16 (the
/// stations' p2p0 carries a link-local address); first of the highest `score` wins.
pub fn best_interface() -> Option<Iface> {
    best_interface_checked().ok().flatten()
}

/// `best_interface`, with Err when getifaddrs failed: the mDNS responder then keeps its
/// current choice, as the C++ one did.
#[allow(clippy::result_unit_err)]
pub fn best_interface_checked() -> Result<Option<Iface>, ()> {
    let addrs = sys::ipv4_ifaddrs().ok_or(())?;
    let def = std::fs::read_to_string("/proc/net/route")
        .ok()
        .and_then(|t| default_route_iface(&t));
    let mut best: Option<(u32, sys::IfAddr)> = None;
    for a in addrs {
        let up = a.flags & libc::IFF_UP as u32 != 0;
        let lo = a.flags & libc::IFF_LOOPBACK as u32 != 0;
        let mc = a.flags & libc::IFF_MULTICAST as u32 != 0;
        if !up || lo || !mc || a.addr.octets()[..2] == [169, 254] {
            continue;
        }
        let s = score(&a.name, def.as_deref());
        if best.as_ref().is_none_or(|(b, _)| s > *b) {
            best = Some((s, a));
        }
    }
    Ok(best.map(|(_, a)| Iface {
        index: sys::if_nametoindex(&a.name),
        name: a.name,
        addr: a.addr,
    }))
}

/// wlan0 = 3 (the stations' Wi-Fi), the default route's interface = 2, anything else = 1.
pub fn score(name: &str, default_route_iface: Option<&str>) -> u32 {
    if name == "wlan0" {
        3
    } else if default_route_iface == Some(name) {
        2
    } else {
        1
    }
}

/// First interface of /proc/net/route with destination 0 and RTF_UP.
fn default_route_iface(table: &str) -> Option<String> {
    table.lines().find_map(|line| {
        let mut f = line.split_whitespace();
        let name = f.next()?;
        let dst = u64::from_str_radix(f.next()?, 16).ok()?;
        let _gw = u64::from_str_radix(f.next()?, 16).ok()?;
        let flags = u64::from_str_radix(f.next()?, 16).ok()?;
        (dst == 0 && flags & 1 != 0).then(|| name.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_prefers_wlan0_then_default_route() {
        assert_eq!(score("wlan0", None), 3);
        assert_eq!(score("wlan0", Some("eth0")), 3);
        assert_eq!(score("eth0", Some("eth0")), 2);
        assert_eq!(score("eth0", Some("usb0")), 1);
        assert_eq!(score("eth0", None), 1);
        assert_eq!(score("", Some("eth0")), 1);
    }

    #[test]
    fn default_route_from_proc_net_route() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     p2p0\t0000FEA9\t00000000\t0001\t0\t0\t0\t0000FFFF\n\
                     wlan0\t00000000\t0101A8C0\t0003\t0\t0\t0\t00000000\n\
                     eth0\t00000000\t0102A8C0\t0002\t0\t0\t0\t00000000\n";
        assert_eq!(default_route_iface(table).as_deref(), Some("wlan0"));
        assert_eq!(
            default_route_iface("Iface\tDestination\tGateway\tFlags\n"),
            None
        );
    }
}
