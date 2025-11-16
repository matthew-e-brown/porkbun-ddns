use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;


/// Current IP address as returned by Porkbun's `/ping` endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ping {
    #[allow(unused)]
    pub x_forwarded_for: IpAddr,
    pub your_ip: IpAddr,
}

/// Response returned from the `/create` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct CreatedRecord {
    pub id: String,
}

/// The list of DNS records returned by Porkbun's `/retrieve` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct DNSRecordList {
    pub records: Vec<DNSRecord>,
}

/// A single Porkbun DNS record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DNSRecord {
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

mod optional_or_stringified_number {
    use serde::de::{Error, Unexpected};
    use serde::{Deserializer, Serializer};

    use super::*;

    /// Deserializes a `u32` which may be a string and which may also not be None.
    pub fn deserialize<'de, D>(d: D) -> Result<Option<u32>, D::Error>
    where
        D: Deserializer<'de>,
        D::Error: serde::de::Error,
    {
        const EXPECTED: &'static str = "integer, string, or null";
        match Option::<JsonValue>::deserialize(d)? {
            None => Ok(None),
            Some(JsonValue::String(str)) if str.as_str().trim().is_empty() => Ok(None),
            Some(JsonValue::String(str)) => Ok(Some(str.parse().map_err(D::Error::custom)?)),
            Some(JsonValue::Number(num)) => {
                let n = num.as_i64().ok_or_else(|| D::Error::custom("not an integer"))?;
                let n = u32::try_from(n).map_err(|_| D::Error::custom("integer out of range"))?;
                Ok(Some(n))
            },
            Some(JsonValue::Null) => return Err(D::Error::invalid_type(Unexpected::Other("null"), &EXPECTED)),
            Some(JsonValue::Bool(v)) => return Err(D::Error::invalid_type(Unexpected::Bool(v), &EXPECTED)),
            Some(JsonValue::Array(_)) => return Err(D::Error::invalid_type(Unexpected::Seq, &EXPECTED)),
            Some(JsonValue::Object(_)) => return Err(D::Error::invalid_type(Unexpected::Map, &EXPECTED)),
        }
    }

    /// Serializes an optional `u32` as a string.
    pub fn serialize<S>(val: &Option<u32>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *val {
            Some(n) => s.serialize_str(&n.to_string()),
            None => s.serialize_none(),
        }
    }
}
