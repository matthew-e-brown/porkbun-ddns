use std::net::{IpAddr, Ipv4Addr};

use anyhow::anyhow;
use chrono::Local;
use reqwest::IntoUrl;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use super::AddressType;
use super::model::{DNSRecord, DNSRecordList, Ping};
use crate::config::DomainJob;


const BASE_URL: &'static str = "https://api.porkbun.com/api/json/v3";
const BASE_URL_V4: &'static str = "https://api-ipv4.porkbun.com/api/json/v3";

type JsonObject = JsonMap<String, JsonValue>;

/// The access point to the Porkbun API.
pub struct PorkbunClient {
    inner: reqwest::Client,
    api_key: String,
    secret_key: String,
}

impl PorkbunClient {
    pub fn new() -> anyhow::Result<Self> {
        let ua_str = format!("{} {}", clap::crate_name!(), clap::crate_version!());
        let client = reqwest::ClientBuilder::new()
            .default_headers(HeaderMap::from_iter([
                (reqwest::header::ACCEPT, HeaderValue::from_static("application/json; charset=utf-8")),
                (reqwest::header::USER_AGENT, HeaderValue::from_str(&ua_str).expect("UA str should be valid")),
            ]))
            .build()
            .unwrap();

        #[inline]
        fn get_env(key: &str) -> anyhow::Result<String> {
            std::env::var(key).or_else(|_| Err(anyhow!("failed to get environment variable {key}")))
        }

        dotenvy::dotenv()?;
        let api_key = get_env("PORKBUN_API_KEY")?;
        let secret_key = get_env("PORKBUN_SECRET_KEY")?;

        Ok(Self {
            inner: client,
            api_key,
            secret_key,
        })
    }

    /// Determine this system's current public IP address using Porkbun's `/ping` endpoint.
    ///
    /// Porkbun may return either an IPv4 or IPv6 address; see [`ping_v4`][Self::ping_v4].
    pub async fn ping(&self) -> anyhow::Result<IpAddr> {
        let url = format!("{BASE_URL}/ping");
        let res = self.request::<Ping>(url, None).await?;
        Ok(res.your_ip)
    }

    /// Determine this system's current IPv4 address using Porkbun's `/ping` endpoint on the `api-ipv4.porkbun.com`
    /// subdomain.
    pub async fn ping_v4(&self) -> anyhow::Result<Ipv4Addr> {
        let url = format!("{BASE_URL_V4}/ping");
        let res = self.request::<Ping>(url, None).await?;
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
        let res = self.request::<DNSRecordList>(url, None).await?;
        Ok(res.records)
    }

    /// Creates the JSON payload for creating or editing a DNS record.
    fn record_payload<A: AddressType>(job: &DomainJob, content: &A::Addr) -> JsonValue {
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
        job: &DomainJob,
        record_id: &str,
        new_content: &A::Addr,
    ) -> anyhow::Result<()> {
        let url = format!("{BASE_URL}/dns/edit/{}/{}", job.domain(), record_id);
        let payload = Self::record_payload::<A>(job, new_content);
        self.request(url, Some(payload)).await
    }

    /// Creates a new DNS record for the given job with the given content.
    pub async fn create_record<A: AddressType>(&self, job: &DomainJob, content: &A::Addr) -> anyhow::Result<()> {
        let url = format!("{BASE_URL}/dns/create/{}", job.domain());
        let payload = Self::record_payload::<A>(job, content);
        self.request(url, Some(payload)).await
    }

    /// Makes a POST request to Porkbun's API and returns the result parsed from JSON.
    async fn request<R>(&self, url: impl IntoUrl, payload: Option<JsonValue>) -> anyhow::Result<R>
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

        let res = self.inner.post(url).json(&payload).send().await?;
        let res = res.json::<JsonValue>().await?;

        // All Porkbun endpoints should return objects with a 'status'
        match res {
            JsonValue::Object(mut map) if matches!(map.get_str("status"), Some("SUCCESS")) => {
                map.remove("status");
                // Now that we've removed 'status', parse the rest of it
                let parsed = serde_json::from_value(JsonValue::Object(map))?;
                Ok(parsed)
            },
            _ => {
                if let JsonValue::Object(ref map) = res
                    && let Some("ERROR") = map.get_str("status")
                    && let Some(msg) = map.get_str("message")
                {
                    Err(anyhow!(msg.to_string()).context("Porkbun API returned an error status"))
                } else {
                    Err(anyhow!(res).context("Porkbun API returned unexpected response"))
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
