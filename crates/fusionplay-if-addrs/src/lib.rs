use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum IfOperStatus {
    Up = 1,
    Down = 2,
    Testing = 3,
    Unknown = 4,
    Dormant = 5,
    NotPresent = 6,
    LowerLayerDown = 7,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Interface {
    pub name: String,
    pub addr: IfAddr,
    pub index: Option<u32>,
    pub oper_status: IfOperStatus,
    pub is_p2p: bool,
    #[cfg(windows)]
    pub adapter_name: String,
}

impl Interface {
    #[must_use]
    pub const fn is_loopback(&self) -> bool {
        self.addr.is_loopback()
    }

    #[must_use]
    pub const fn is_link_local(&self) -> bool {
        self.addr.is_link_local()
    }

    #[must_use]
    pub const fn ip(&self) -> IpAddr {
        self.addr.ip()
    }

    #[must_use]
    pub fn is_oper_up(&self) -> bool {
        self.oper_status == IfOperStatus::Up
    }

    #[must_use]
    pub const fn is_p2p(&self) -> bool {
        self.is_p2p
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum IfAddr {
    V4(Ifv4Addr),
    V6(Ifv6Addr),
}

impl IfAddr {
    #[must_use]
    pub const fn is_loopback(&self) -> bool {
        match self {
            Self::V4(address) => address.is_loopback(),
            Self::V6(address) => address.is_loopback(),
        }
    }

    #[must_use]
    pub const fn is_link_local(&self) -> bool {
        match self {
            Self::V4(address) => address.is_link_local(),
            Self::V6(address) => address.is_link_local(),
        }
    }

    #[must_use]
    pub const fn ip(&self) -> IpAddr {
        match self {
            Self::V4(address) => IpAddr::V4(address.ip),
            Self::V6(address) => IpAddr::V6(address.ip),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Ifv4Addr {
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub prefixlen: u8,
    pub broadcast: Option<Ipv4Addr>,
}

impl Ifv4Addr {
    #[must_use]
    pub const fn is_loopback(&self) -> bool {
        self.ip.is_loopback()
    }

    #[must_use]
    pub const fn is_link_local(&self) -> bool {
        self.ip.is_link_local()
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Ifv6Addr {
    pub ip: Ipv6Addr,
    pub netmask: Ipv6Addr,
    pub prefixlen: u8,
    pub broadcast: Option<Ipv6Addr>,
}

impl Ifv6Addr {
    #[must_use]
    pub const fn is_loopback(&self) -> bool {
        self.ip.is_loopback()
    }

    #[must_use]
    pub const fn is_link_local(&self) -> bool {
        self.ip.is_unicast_link_local()
    }
}

pub fn get_if_addrs() -> io::Result<Vec<Interface>> {
    let networks = getifs::interface_addrs()?;
    let mut interfaces = Vec::with_capacity(networks.len());

    for network in networks {
        let index = network.index();
        let name = network
            .name()
            .map(|value| value.to_string())
            .unwrap_or_else(|_| format!("if{index}"));
        let prefixlen = network.prefix_len();
        let addr = match network.addr() {
            IpAddr::V4(ip) => {
                let netmask_bits = if prefixlen == 0 {
                    0
                } else {
                    u32::MAX << (32 - u32::from(prefixlen))
                };
                let ip_bits = u32::from(ip);
                IfAddr::V4(Ifv4Addr {
                    ip,
                    netmask: Ipv4Addr::from(netmask_bits),
                    prefixlen,
                    broadcast: Some(Ipv4Addr::from(ip_bits | !netmask_bits)),
                })
            }
            IpAddr::V6(ip) => {
                let netmask_bits = if prefixlen == 0 {
                    0
                } else {
                    u128::MAX << (128 - u32::from(prefixlen))
                };
                IfAddr::V6(Ifv6Addr {
                    ip,
                    netmask: Ipv6Addr::from(netmask_bits),
                    prefixlen,
                    broadcast: None,
                })
            }
        };

        let interface_details = getifs::interface_by_index(index).ok().flatten();
        let (oper_status, is_p2p) = interface_details
            .map(|details| {
                let flags = details.flags();
                (
                    if flags.contains(getifs::Flags::UP) {
                        IfOperStatus::Up
                    } else {
                        IfOperStatus::Down
                    },
                    flags.contains(getifs::Flags::POINTOPOINT),
                )
            })
            .unwrap_or((IfOperStatus::Unknown, false));

        interfaces.push(Interface {
            name,
            addr,
            index: Some(index),
            oper_status,
            is_p2p,
            #[cfg(windows)]
            adapter_name: String::new(),
        });
    }

    Ok(interfaces)
}

/// Returns the link-layer (MAC) address of the interface that owns `address`.
///
/// Kept as a free function rather than an [`Interface`] field so downstream
/// crates that build `Interface` values (for example `mdns-sd`) keep compiling
/// against the patched `if-addrs` API.
///
/// MiPlay discovery has to advertise this value in the `_mi-connect` TXT
/// record: Xiaomi's BonjourGovernor reads `mac` first and only falls back to a
/// ping plus neighbour-table lookup, which cannot resolve a receiver that sits
/// behind a wireless-to-wired bridge.
pub fn hardware_address_for(address: IpAddr) -> io::Result<Option<[u8; 6]>> {
    let networks = getifs::interface_addrs()?;
    let Some(network) = networks
        .iter()
        .find(|network| network.addr() == address)
    else {
        return Ok(None);
    };
    let Some(details) = getifs::interface_by_index(network.index()).ok().flatten() else {
        return Ok(None);
    };
    let Some(mac) = details.mac_addr() else {
        return Ok(None);
    };
    let bytes = mac.as_bytes();
    if bytes.len() != 6 {
        return Ok(None);
    }
    let mut hardware_address = [0_u8; 6];
    hardware_address.copy_from_slice(bytes);
    Ok(Some(hardware_address))
}
