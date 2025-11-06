mod client;
pub mod model;

use std::fmt::Display;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use self::client::PorkbunClient;


/// Marker trait for two implementing different functionality for [`IPv4`] and [`IPv6`] addresses.
pub trait AddressType {
    /// Which [`std::net`] type this address type turns into.
    type Addr: Serialize + DeserializeOwned + Display;

    /// Which type of DNS record this type uses.
    const RECORD_TYPE: &'static str;
}

/// The [`AddressType`] that corresponds to [`std::net::Ipv4Addr`].
pub struct IPv4;

/// The [`AddressType`] that corresponds to [`std::net::Ipv6Addr`].
pub struct IPv6;

impl AddressType for IPv4 {
    type Addr = Ipv4Addr;
    const RECORD_TYPE: &'static str = "A";
}

impl AddressType for IPv6 {
    type Addr = Ipv6Addr;
    const RECORD_TYPE: &'static str = "AAAA";
}
