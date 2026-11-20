//! The guest's link: one Ethernet interface on a network that only the guest
//! and `ap` are on. `ap` is its gateway and its only resolver, at one address
//! for each family. The init sets it all up itself, with no DHCP.

use std::net::{Ipv4Addr, Ipv6Addr};

/// The interface the kernel names the only Ethernet device.
pub const INTERFACE: &str = "eth0";
/// The guest's MAC address. It is locally administered, so it is no
/// vendor's.
pub const MAC: [u8; 6] = [0x02, 0x61, 0x70, 0x00, 0x00, 0x0f];

/// `ap`'s MAC address, the gateway's. It is locally administered too.
pub const GATEWAY_MAC: [u8; 6] = [0x02, 0x61, 0x70, 0x00, 0x00, 0x02];

pub const IPV4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
pub const IPV4_PREFIX: u8 = 24;
/// `ap`'s address, the guest's gateway and resolver.
pub const IPV4_GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// A unique local address, which is never routed on the internet.
pub const IPV6: Ipv6Addr = Ipv6Addr::new(0xfd61, 0x7000, 0, 0, 0, 0, 0, 0x15);
pub const IPV6_PREFIX: u8 = 64;
/// `ap`'s address, the guest's gateway and resolver.
pub const IPV6_GATEWAY: Ipv6Addr = Ipv6Addr::new(0xfd61, 0x7000, 0, 0, 0, 0, 0, 2);

/// The guest's `/etc/resolv.conf`, which names `ap` alone.
pub fn resolv_conf() -> String {
    format!("nameserver {IPV4_GATEWAY}\nnameserver {IPV6_GATEWAY}\n")
}

/// Whether [`IPV6`] is still tentative, as `/proc/net/if_inet6` lists it,
/// or `None` if it doesn't list it.
pub fn tentative(if_inet6: &str) -> Option<bool> {
    const TENTATIVE: u32 = 0x40;
    let address: String = IPV6
        .octets()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if_inet6.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let flags = u32::from_str_radix(fields.get(4)?, 16).ok()?;
        (fields[0] == address).then_some(flags & TENTATIVE != 0)
    })
}

/// The netmask of [`IPV4_PREFIX`].
pub fn ipv4_netmask() -> Ipv4Addr {
    Ipv4Addr::from_bits(u32::MAX << (32 - IPV4_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gateways_share_the_guest_s_links() {
        let mask = ipv4_netmask().to_bits();
        assert_eq!(ipv4_netmask(), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(IPV4.to_bits() & mask, IPV4_GATEWAY.to_bits() & mask);
        assert_ne!(IPV4, IPV4_GATEWAY);
        let mask = u128::MAX << (128 - IPV6_PREFIX);
        assert_eq!(IPV6.to_bits() & mask, IPV6_GATEWAY.to_bits() & mask);
        assert_ne!(IPV6, IPV6_GATEWAY);
        assert!(IPV6.is_unique_local());
        assert!(IPV4.is_private());
        assert_eq!(MAC[0] & 0b11, 0b10, "locally administered and unicast");
    }

    #[test]
    fn the_address_is_tentative_until_the_kernel_says_otherwise() {
        let link_local = "fe80000000000000006170fffe00000f 02 40 20 80     eth0\n";
        let waiting = "fd617000000000000000000000000015 02 40 00 c0     eth0\n";
        let ready = "fd617000000000000000000000000015 02 40 00 80     eth0\n";
        assert_eq!(tentative(""), None);
        assert_eq!(tentative(link_local), None);
        assert_eq!(tentative(&format!("{link_local}{waiting}")), Some(true));
        assert_eq!(tentative(&format!("{link_local}{ready}")), Some(false));
        assert_eq!(tentative("fd617000000000000000000000000015 02 40"), None);
    }

    #[test]
    fn ap_is_the_only_resolver() {
        assert_eq!(
            resolv_conf(),
            "nameserver 10.0.2.2\nnameserver fd61:7000::2\n"
        );
    }
}
