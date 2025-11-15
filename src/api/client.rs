use std::net::{IpAddr, Ipv4Addr};

use anyhow::{Context, anyhow};
use chrono::Local;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use super::model::{DNSRecord, DNSRecordList, Ping};
use super::{AddressType, BASE_URL, BASE_URL_V4};
use crate::config::Target;

type JsonObject = JsonMap<String, JsonValue>;

/// The access point to the Porkbun API.
#[derive(Debug)]
pub struct PorkbunClient {
    reqwest: reqwest::Client,
    api_key: String,
    secret_key: String,
}

impl PorkbunClient {
    pub fn new(api_key: String, secret_key: String) -> Self {
        let ua_str = format!("{} {}", clap::crate_name!(), clap::crate_version!());
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
    pub async fn ping(&self) -> anyhow::Result<IpAddr> {
        let url = format!("{BASE_URL}/ping");
        let res = self.request::<Ping>(&url, None).await?;
        Ok(res.your_ip)
    }

    /// Determine this system's current IPv4 address using Porkbun's `/ping` endpoint on the `api-ipv4.porkbun.com`
    /// subdomain.
    pub async fn ping_v4(&self) -> anyhow::Result<Ipv4Addr> {
        let url = format!("{BASE_URL_V4}/ping");
        let res = self.request::<Ping>(&url, None).await?;
        // What happens if a system *only* has IPv6? Will /ping return an error?
        // ...I don't really have a way to test that.
        match res.your_ip {
            IpAddr::V4(addr) => Ok(addr),
            IpAddr::V6(addr) => Err(anyhow!("IPv4 ping returned IPv6 address: {addr}")),
        }
    }

    /// Gets all the existing records for the given domain name.
    pub async fn get_existing_records(&self, domain: &str) -> anyhow::Result<Vec<DNSRecord>> {
        let url = format!("{BASE_URL}/dns/retrieve/{domain}");
        let res = self.request::<DNSRecordList>(&url, None).await?;
        Ok(res.records)
    }

    /// Creates the JSON payload for creating or editing a DNS record.
    fn make_dns_payload<A: AddressType>(job: &Target, content: &A::Addr) -> JsonValue {
        let time = Local::now().format("%a %b %-d %Y at %I:%M:%S %p");
        json!({
            "name": match job.subdomain() {
                Some("@") | None => "",
                Some(sub) => sub,
            },
            "type": A::RECORD_TYPE,
            "content": content,
            "ttl": job.ttl(),
            "notes": format!("Last updated by {} on {time}", clap::crate_name!()),
        })
    }

    /// Edits a record for the given job with the given ID to have the given content.
    pub async fn edit_record<A: AddressType>(
        &self,
        job: &Target,
        record_id: &str,
        new_content: &A::Addr,
    ) -> anyhow::Result<()> {
        let url = format!("{BASE_URL}/dns/edit/{}/{}", job.domain(), record_id);
        let payload = Self::make_dns_payload::<A>(job, new_content);
        self.request(&url, Some(payload)).await
    }

    /// Creates a new DNS record for the given job with the given content.
    pub async fn create_record<A: AddressType>(&self, job: &Target, content: &A::Addr) -> anyhow::Result<()> {
        let url = format!("{BASE_URL}/dns/create/{}", job.domain());
        let payload = Self::make_dns_payload::<A>(job, content);
        self.request(&url, Some(payload)).await
    }

    /// Makes a POST request to Porkbun's API and returns the result parsed from JSON.
    async fn request<R>(&self, url: &str, payload: Option<JsonValue>) -> anyhow::Result<R>
    where
        R: DeserializeOwned,
    {
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
            .context("failed to send POST request")?;
        let res_text = res_raw.text().await.context("failed to read POST response")?;
        let res_json = serde_json::from_str(&res_text[..]).context("Porkbun API invalid JSON")?;

        // All Porkbun endpoints should return objects with a 'status'
        match res_json {
            JsonValue::Object(mut map) if map.get_str("status") == Some("SUCCESS") => {
                map.remove("status");
                // Now that we've removed 'status', parse the rest of it
                let parsed = serde_json::from_value(JsonValue::Object(map)).context("failed to parse JSON response")?;
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

                    Err(anyhow!(msg).context("Porkbun API returned an error"))
                } else {
                    Err(anyhow!(json).context("Porkbun API returned an invalid/unexpected response"))
                }
            },
        }
    }
}

trait JsonObjectExt {
    /// Combines [`JsonMap::get`] and [`JsonValue::as_str`] into one method that only returns the value if it both
    /// exists and is a string.
    fn get_str(&self, key: &str) -> Option<&str>;
}

impl JsonObjectExt for JsonObject {
    #[inline]
    fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(JsonValue::as_str)
    }
}
