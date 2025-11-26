use std::net::{IpAddr, Ipv4Addr};
use std::sync::LazyLock;

use chrono::Local;
use eyre::{WrapErr, eyre};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use super::model::{CreatedRecord, DNSRecord, DNSRecordList, Ping};
use super::{BASE_URL, BASE_URL_V4, IpAddrExt};
use crate::config::Target;

/// Main access point to the Porkbun API.
#[derive(Debug)]
pub struct PorkbunClient {
    reqwest: reqwest::Client,
    api_key: String,
    secret_key: String,
}

/// Timestamp format for DNS records. Format is `Sun Jul 8 2001 at 8:46:23 PM`.
static TIMESTAMP_FMT: LazyLock<&'static [chrono::format::Item<'static>]> = LazyLock::new(|| {
    // NB: `LazyLock`'s own docs have a note about how static items don't ever get dropped, so leaking this Vec into a
    // static slice doesn't make any difference in that regard.
    chrono::format::StrftimeItems::new("%a %b %-d %Y at %-I:%M:%S %p")
        .parse_to_owned()
        .expect("hardcoded strftime string should be valid")
        .leak()
});

impl PorkbunClient {
    pub fn new(api_key: String, secret_key: String) -> Self {
        let ua_str = format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        let client = reqwest::ClientBuilder::new()
            .default_headers(HeaderMap::from_iter([
                (reqwest::header::ACCEPT, HeaderValue::from_static("application/json; charset=utf-8")),
                (reqwest::header::USER_AGENT, HeaderValue::from_str(&ua_str).expect("UA str should be valid")),
            ]))
            .build()
            .unwrap();

        Self {
            reqwest: client,
            api_key,
            secret_key,
        }
    }

    /// Determine this system's current public IP address using Porkbun's `/ping` endpoint.
    ///
    /// Porkbun may return either an IPv4 or IPv6 address; see [`ping_v4`][Self::ping_v4].
    pub async fn ping(&self) -> eyre::Result<IpAddr> {
        log::trace!("Sending ping");
        let url = format!("{BASE_URL}/ping");
        let res = self.request::<Ping>(&url, None).await?;
        Ok(res.your_ip)
    }

    /// Determine this system's current IPv4 address using Porkbun's `/ping` endpoint on the `api-ipv4.porkbun.com`
    /// subdomain.
    pub async fn ping_v4(&self) -> eyre::Result<Ipv4Addr> {
        log::trace!("Sending ping to IPv4 endpoint");
        let url = format!("{BASE_URL_V4}/ping");
        let res = self.request::<Ping>(&url, None).await?;
        // What happens if a system *only* has IPv6? Will the IPv4 /ping return an error?
        // ...I don't really have a way to test that.
        match res.your_ip {
            IpAddr::V4(addr) => Ok(addr),
            IpAddr::V6(addr) => Err(eyre!("IPv4-only ping somehow returned IPv6 address {addr}")),
        }
    }

    /// Gets all the existing records for the given domain name.
    pub async fn get_existing_records(&self, domain: &str) -> eyre::Result<Vec<DNSRecord>> {
        log::trace!("Getting existing records for domain {domain}");
        let url = format!("{BASE_URL}/dns/retrieve/{domain}");
        let res = self.request::<DNSRecordList>(&url, None).await?;
        Ok(res.records)
    }

    /// Creates the JSON payload for creating or editing a DNS record.
    fn make_dns_payload(target: &Target, content: IpAddr) -> JsonValue {
        let timestamp = Local::now().format_with_items(TIMESTAMP_FMT.iter());
        json!({
            // In both create and edit payloads, the `name` field only includes the subdomain:
            // - https://porkbun.com/api/json/v3/documentation#DNS%20Create%20Record
            // - https://porkbun.com/api/json/v3/documentation#DNS%20Edit%20Record%20by%20Domain%20and%20ID
            "name": match target.subdomain() {
                Some("@") | None => "",
                Some(sub) => sub,
            },
            "type": content.dns_type(),
            "content": content,
            "ttl": target.ttl(),
            "notes": format!("Last updated by {} on {timestamp}", env!("CARGO_PKG_NAME")),
        })
    }

    /// Edits an existing record for the given target.
    ///
    /// `record_id` must be fetched beforehand. It is not double checked to match Porkbun's API status before sending
    /// the request.
    pub async fn edit_record(&self, target: &Target, record_id: &str, new_content: IpAddr) -> eyre::Result<()> {
        log::trace!("Editing record {record_id} for target {target} with new content \"{new_content}\"");
        let url = format!("{BASE_URL}/dns/edit/{}/{}", target.domain(), record_id);
        let payload = Self::make_dns_payload(target, new_content);
        self.request::<()>(&url, Some(payload)).await
    }

    /// Creates a new DNS record for the given target with the given content.
    ///
    /// Returns the ID of the newly created record.
    pub async fn create_record(&self, target: &Target, content: IpAddr) -> eyre::Result<String> {
        log::trace!("Creating new record for target {target} with new content \"{content}\"");
        let url = format!("{BASE_URL}/dns/create/{}", target.domain());
        let payload = Self::make_dns_payload(target, content);
        let res = self.request::<CreatedRecord>(&url, Some(payload)).await?;
        Ok(res.id)
    }

    /// Makes a POST request to Porkbun's API and returns the result parsed from JSON.
    async fn request<R>(&self, url: &str, payload: Option<JsonValue>) -> eyre::Result<R>
    where
        R: DeserializeOwned,
    {
        log::trace!("Sending POST request to {url} with payload {payload:?}");

        let mut payload = match payload {
            Some(JsonValue::Object(map)) => map,
            Some(JsonValue::Null) | None => JsonMap::<String, JsonValue>::new(),
            Some(other) => panic!("JSON payload must be a map/object, got `{other:?}`."),
        };

        payload.insert("apikey".to_string(), json!(self.api_key));
        payload.insert("secretapikey".to_string(), json!(self.secret_key));

        // Send the request and get its response as raw text before parsing it to JSON ourselves; lets us be more
        // precise with our error handling.
        let res_raw = self
            .reqwest
            .post(url)
            .json(&payload)
            .send()
            .await
            .wrap_err("POST request failed")?;

        let res_text = res_raw.text().await.wrap_err("Failed to read POST response body")?;
        log::trace!("Received POST response of size {} bytes", res_text.len());

        let res_json = serde_json::from_str(&res_text[..]).wrap_err("Received invalid JSON from Porkbun API")?;

        // All Porkbun endpoints should return objects with a 'status'
        match res_json {
            JsonValue::Object(mut map) if map.get_str("status") == Some("SUCCESS") => {
                map.remove("status");
                // Now that we've removed 'status', parse the rest of it
                let parsed = serde_json::from_value(JsonValue::Object(map))
                    .wrap_err("Could not parse JSON response from Porkbun API into valid type")?;
                Ok(parsed)
            },
            mut json => {
                if let JsonValue::Object(ref mut map) = json
                    && map.get_str("status") == Some("ERROR")
                    && map.get("message").is_some_and(JsonValue::is_string)
                {
                    let Some(JsonValue::String(msg)) = map.remove("message") else {
                        // We already matched for this exact thing, we just didn't want to remove the owned copy of the
                        // string until all checks had passed.
                        unreachable!();
                    };

                    Err(eyre!("Received error from Porkbun API: \"{msg}\""))
                } else {
                    Err(eyre!("Received unexpected response from Porkbun API: {json}"))
                }
            },
        }
    }
}

/// Extension trait for Json `{}` objects.
trait JsonObjectExt {
    /// Combines [`JsonMap::get`] and [`JsonValue::as_str`] into one method that only returns the value if it both
    /// exists and is a string.
    fn get_str(&self, key: &str) -> Option<&str>;
}

impl JsonObjectExt for JsonMap<String, JsonValue> {
    #[inline]
    fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(JsonValue::as_str)
    }
}
