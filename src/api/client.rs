use std::net::{IpAddr, Ipv4Addr};
use std::sync::LazyLock;

use chrono::Local;
use eyre::{WrapErr, eyre};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use super::model::{CreateResponse, DNSRecord, EditResponse, PingResponse, RetrieveResponse};
use super::{BASE_URL, BASE_URL_V4, IpAddrExt};
use crate::config::Target;

/// The main entrypoint for the Porkbun API.
#[derive(Debug)]
pub struct PorkbunClient {
    reqwest: reqwest::Client,
    api_key: String,
    secret_key: String,
}

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
        let res = self.request::<PingResponse>(&url, None).await?;
        Ok(res.your_ip)
    }

    /// Determine this system's current IPv4 address using Porkbun's `/ping` endpoint on the `api-ipv4.porkbun.com`
    /// subdomain.
    pub async fn ping_v4(&self) -> eyre::Result<Ipv4Addr> {
        log::trace!("Sending ping to IPv4 endpoint");
        let url = format!("{BASE_URL_V4}/ping");
        let res = self.request::<PingResponse>(&url, None).await?;
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
        let res = self.request::<RetrieveResponse>(&url, None).await?;
        Ok(res.records)
    }

    /// Edits an existing record for the given target.
    ///
    /// `record_id` must be fetched beforehand. It is not double checked to match Porkbun's API status before sending
    /// the request.
    pub async fn edit_record(&self, target: &Target, record_id: &str, new_content: IpAddr) -> eyre::Result<()> {
        log::trace!("Editing record {record_id} for target {target} with new content \"{new_content}\"");
        let url = format!("{BASE_URL}/dns/edit/{}/{}", target.domain(), record_id);
        let payload = make_dns_payload(target, new_content);
        let _res = self.request::<EditResponse>(&url, Some(payload)).await?;
        Ok(())
    }

    /// Creates a new DNS record for the given target with the given content.
    ///
    /// Returns the ID of the newly created record.
    pub async fn create_record(&self, target: &Target, content: IpAddr) -> eyre::Result<String> {
        log::trace!("Creating new record for target {target} with new content \"{content}\"");
        let url = format!("{BASE_URL}/dns/create/{}", target.domain());
        let payload = make_dns_payload(target, content);
        let res = self.request::<CreateResponse>(&url, Some(payload)).await?;
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
            Some(JsonValue::Null) | None => JsonMap::new(),
            Some(other) => panic!("JSON payload must be a map/object, got `{other:?}`"),
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
        log::trace!("Received POST response with body: {}", res_text);

        match parse_response(&res_text[..]) {
            Ok(Ok(parsed)) => Ok(parsed),
            Ok(Err(err)) => Err(err),
            Err(err) => Err(eyre!("{err:#}. Raw response: {res_text}")),
        }
    }
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

/// Creates a JSON payload for creating or editing a DNS record for the given target.
fn make_dns_payload(target: &Target, addr: IpAddr) -> JsonValue {
    let timestamp = Local::now().format_with_items(TIMESTAMP_FMT.iter());
    json!({
        // In both create and edit payloads, the `name` field only includes the subdomain, since the domain itself is a
        // path parameter within the URL:
        // - https://porkbun.com/api/json/v3/documentation#DNS%20Create%20Record
        // - https://porkbun.com/api/json/v3/documentation#DNS%20Edit%20Record%20by%20Domain%20and%20ID
        "name": match target.subdomain() {
            Some("@") | None => "",
            Some(sub) => sub,
        },
        "type": addr.dns_type(),
        "content": addr,
        "ttl": target.ttl(),
        "notes": format!("Last updated by {} on {timestamp}", env!("CARGO_PKG_NAME")),
    })
}

/// Attempts to parse/deserialize Porkbun's API responses into the right type.
///
/// - Returns `Ok(Ok(R))` if a successful response was successfully parsed.
/// - Returns `Ok(Err(_))` if an error response was successfully parsed.
/// - Returns `Err(_)` if neither response could be parsed.
fn parse_response<R: DeserializeOwned>(body: &str) -> Result<eyre::Result<R>, eyre::Report> {
    let json = serde_json::from_str(body).wrap_err("Response was not valid JSON")?;
    // All Porkbun endpoints *should* return objects with a 'status' key of either "SUCCESS" or "ERROR". Error responses
    // *should* all have a "message" key on them.
    match json {
        JsonValue::Object(mut map) if map.get("status").and_then(JsonValue::as_str) == Some("SUCCESS") => {
            // Remove the 'status' key and then attempt to parse the final type from the object:
            map.remove("status");
            let parsed = serde_json::from_value(JsonValue::Object(map))
                .wrap_err("Response was successful, but was not the expected type")?;
            Ok(Ok(parsed))
        },
        JsonValue::Object(map)
            if map.get("status").and_then(JsonValue::as_str) == Some("ERROR")
                && map.get("message").is_some_and(JsonValue::is_string) =>
        {
            let msg = map.get("message").and_then(JsonValue::as_str).unwrap();
            Ok(Err(eyre!("Received error from Porkbun API: \"{msg}\"")))
        },
        _ => Err(eyre!("Response was in an unknown format")),
    }
}
