use std::net::IpAddr;

use eyre::eyre;
use serde::{Deserialize, Serialize};

use super::IpAddrExt;

/// Response returned by Porkbun's `/ping` endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PingResponse {
    #[allow(dead_code)]
    pub x_forwarded_for: IpAddr,
    pub your_ip: IpAddr,
}

/// Response returned by Porkbun's `/create` endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateResponse {
    #[serde(with = "primitive_as_string")]
    pub id: String,
}

/// Response returned by Porkbun's `/edit` endpoint.
///
/// The `/edit` endpoint's responses have no fields other than the base `status` field. An empty, but non-unit, struct
/// is needed to get serde to parse the response correctly.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditResponse {}

/// Response returned by Porkbun's `/retrieve` endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrieveResponse {
    pub records: Vec<DNSRecord>,
}

/// A single Porkbun DNS record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DNSRecord {
    #[serde(with = "primitive_as_string")]
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub content: String,
    #[serde(with = "optional_or_stringified_number")]
    pub ttl: Option<u32>,
    #[serde(with = "optional_or_stringified_number")]
    pub prio: Option<u32>,
    pub notes: Option<String>,
}

impl DNSRecord {
    /// Attempts to parse an IP address out of this DNS record's [`content`][Self::content] field.
    ///
    /// Returns an error if the IP address is not valid, if this is not an A/AAAA record, or if the type of IP address
    /// does not match what is expected for the record's type.
    pub fn try_parse_ip(&self) -> eyre::Result<IpAddr> {
        if !(self.typ == "A" || self.typ == "AAAA") {
            return Err(eyre!("cannot parse IP address from record with type {}", self.typ));
        }

        let addr = self.content.parse::<IpAddr>()?;
        if addr.dns_type() != self.typ {
            let exp = if self.typ == "A" { "IPv4" } else { "IPv6" };
            let acc = if addr.is_ipv4() { "IPv4" } else { "IPv6" };
            Err(eyre!("record of type {} has the wrong IP address type (should have {exp}, has {acc})", self.typ))
        } else {
            Ok(addr)
        }
    }
}

// [NOTE] Providing whole `Visitor` implementations for both of the following is kinda way overcomplicated for what we
// need. There are simpler ways this could have been done. But this was a great opportunity to get more comfortable with
// serde, so I went with it!

/// A `serde(with)` module that handles a `u32` which may or may not be present, and which may or may not be
/// stringified. Serialization always serializes into `Some(u32)`.
mod optional_or_stringified_number {
    use serde::{Deserializer, Serializer, de};

    #[derive(Debug)]
    struct Visitor;

    impl Visitor {
        /// Tries to convert the given value into a `u32`. If the conversion fails for any reason, the error message is
        /// always "integer out of range".
        fn try_int<T: TryInto<u32>, E: de::Error>(self, x: T) -> Result<u32, E> {
            x.try_into().map_err(|_| de::Error::custom("integer out of range"))
        }
    }

    #[rustfmt::skip]
    impl<'de> de::Visitor<'de> for Visitor {
        type Value = Option<u32>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an integer, a string, or null")
        }

        fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
            // Treat `Some(x)` simply as `x`.
            deserializer.deserialize_any(self)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            // `serde_json` serializes units to null; match that behaviour. If the deserializer finds a unit, this
            // visitor pretends it just found a `null` and treats it as `None`.
            self.visit_none()
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            if v.is_empty() {
                Ok(None)
            } else {
                let i = v.parse::<i64>().map_err(de::Error::custom)?;
                self.try_int(i).map(Some)
            }
        }

        fn visit_i128<E: de::Error>(self, v: i128) -> Result<Self::Value, E> { self.try_int(v).map(Some) }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> { self.try_int(v).map(Some) }
        fn visit_i32<E: de::Error>(self, v: i32) -> Result<Self::Value, E> { self.try_int(v).map(Some) }
        fn visit_i16<E: de::Error>(self, v: i16) -> Result<Self::Value, E> { self.try_int(v).map(Some) }
        fn visit_i8<E: de::Error>(self, v: i8) -> Result<Self::Value, E> { self.try_int(v).map(Some) }
        fn visit_u128<E: de::Error>(self, v: u128) -> Result<Self::Value, E> { self.try_int(v).map(Some) }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> { self.try_int(v).map(Some) }
        fn visit_u32<E: de::Error>(self, v: u32) -> Result<Self::Value, E> { Ok(Some(v)) }
        fn visit_u16<E: de::Error>(self, v: u16) -> Result<Self::Value, E> { Ok(Some(v as u32)) }
        fn visit_u8<E: de::Error>(self, v: u8) -> Result<Self::Value, E> { Ok(Some(v as u32)) }
    }

    /// Deserializes a `u32` which may be a string and which may also not be `None`.
    pub fn deserialize<'de, D>(d: D) -> Result<Option<u32>, D::Error>
    where
        D: Deserializer<'de>,
        D::Error: de::Error,
    {
        d.deserialize_any(Visitor)
    }

    /// Serializes an optional `u32`.
    pub fn serialize<S>(val: &Option<u32>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *val {
            Some(n) => s.serialize_u32(n),
            None => s.serialize_none(),
        }
    }
}

/// A `serde(with)` module that supports deserializing any primitive type into a string.
mod primitive_as_string {
    use serde::{Deserializer, Serializer, de};

    #[derive(Debug)]
    struct Visitor;

    #[rustfmt::skip]
    impl<'de> de::Visitor<'de> for Visitor {
        type Value = String;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a primitive value or a string")
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> { Ok(v) }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_char<E: de::Error>(self, v: char) -> Result<Self::Value, E> { Ok(v.to_string()) }

        fn visit_u128<E: de::Error>(self, v: u128) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_u32<E: de::Error>(self, v: u32) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_u16<E: de::Error>(self, v: u16) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_u8<E: de::Error>(self, v: u8) -> Result<Self::Value, E> { Ok(v.to_string()) }

        fn visit_i128<E: de::Error>(self, v: i128) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_i32<E: de::Error>(self, v: i32) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_i16<E: de::Error>(self, v: i16) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_i8<E: de::Error>(self, v: i8) -> Result<Self::Value, E> { Ok(v.to_string()) }

        fn visit_f32<E: de::Error>(self, v: f32) -> Result<Self::Value, E> { Ok(v.to_string()) }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> { Ok(v.to_string()) }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Ok(v.then_some("true").unwrap_or("false").to_string())
        }
    }

    pub fn deserialize<'de, D>(d: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
        D::Error: de::Error,
    {
        d.deserialize_any(Visitor)
    }

    pub fn serialize<S>(val: &str, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(val)
    }
}
