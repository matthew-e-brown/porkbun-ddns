mod client;
pub mod model;

use std::fmt::Display;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use self::client::PorkbunClient;

const BASE_URL: &'static str = "https://api.porkbun.com/api/json/v3";
const BASE_URL_V4: &'static str = "https://api-ipv4.porkbun.com/api/json/v3";

/// Marker trait for two implementing different functionality for [`IPv4`] and [`IPv6`] addresses.
pub trait AddressType
where
    <Self::Addr as FromStr>::Err: std::error::Error,
{
    /// Which [`std::net`] type this address type turns into.
    type Addr: Serialize + DeserializeOwned + FromStr + Display + PartialEq + Eq + Clone;

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
