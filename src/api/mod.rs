mod client;
mod model;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub use self::client::PorkbunClient;
pub use self::model::DNSRecord;

const BASE_URL: &'static str = "https://api.porkbun.com/api/json/v3";
const BASE_URL_V4: &'static str = "https://api-ipv4.porkbun.com/api/json/v3";

pub trait IpAddrExt {
    /// Gets the type of DNS record associated with this IP address type.
    fn dns_type(&self) -> &'static str;
}

impl IpAddrExt for Ipv4Addr {
    fn dns_type(&self) -> &'static str {
        "A"
    }
}

impl IpAddrExt for Ipv6Addr {
    fn dns_type(&self) -> &'static str {
        "AAAA"
    }
}

impl IpAddrExt for IpAddr {
    fn dns_type(&self) -> &'static str {
        match self {
            IpAddr::V4(addr) => addr.dns_type(),
            IpAddr::V6(addr) => addr.dns_type(),
        }
    }
}
