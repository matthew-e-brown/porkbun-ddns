use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use serde::{Deserialize, Deserializer, de};
use tokio::fs;

use crate::api::model::DNSRecord;


// [FIXME] Serde does not support literals as default values yet: https://github.com/serde-rs/serde/issues/368
#[rustfmt::skip] const fn bool<const X: bool>() -> bool { X }
#[rustfmt::skip] const fn empty<T>() -> Vec<T> { Vec::new() }


#[derive(Debug, clap::Parser)]
#[command(version, about, max_term_width = 100)]
struct Args {
    /// Fetch current IP addresses and determine which records to update, but don't actually update
    /// anything.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Path to TOML file containing configuration for the domains to update.
    #[arg(
        short,
        long,
        env = "PORKBUN_DDNS_CONFIG",
        value_name = "FILE",
        default_value = "/etc/ddns.toml"
    )]
    cfg: PathBuf,
}

/// Main program configuration and job specification.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Fetch addresses and determine which records to update, but don't actually update.
    #[serde(default = "bool::<false>", skip)] // Not set by serde; passed through from CLI args
    pub dry_run: bool,

    /// Enables updating of `A` records with an IPv4 address.
    #[serde(default = "bool::<true>")]
    pub ipv4: bool,

    /// Enables updating of `AAAA` records with an IPv6 address.
    #[serde(default = "bool::<false>")]
    pub ipv6: bool,

    /// When this option is false (the default), `AAAA` records are updated "if possible:" failure to acquire an IPv6
    /// address when pinging Porkbun will not cause an error. When this option is true, failure to acquire an IPv6
    /// address will trigger a fatal error.
    #[serde(default = "bool::<false>")]
    pub ipv6_error: bool,

    /// A list of domains to update.
    #[serde(default = "empty")]
    pub domains: Vec<DomainJob>,
}

impl Config {
    /// Loads runtime configuration from command line arguments and configuration file.
    pub async fn init() -> anyhow::Result<Self> {
        let args = Args::parse();

        let text = fs::read_to_string(&args.cfg).await.context("failed to read config file.")?;
        let mut config: Config = toml::from_str(&text).context("failed to parse config file.")?;

        config.extend_from_args(&args);

        Ok(config)
    }

    /// Copies over non-TOML settings from the command line into this [`Config`] struct.
    fn extend_from_args(&mut self, args: &Args) {
        self.dry_run = args.dry_run;
        // ...other future settings.
    }
}


/// Job specification for a single domain or subdomain to update.
#[derive(Debug)]
pub struct DomainJob {
    domain: String,
    subdomain: Option<String>,
    ttl: u32,
}

impl DomainJob {
    pub fn domain(&self) -> &str {
        &self.domain[..]
    }

    pub fn subdomain(&self) -> Option<&str> {
        self.subdomain.as_deref()
    }

    pub fn ttl(&self) -> u32 {
        self.ttl
    }

    /// Creates a default [`DomainJob`] out of just a domain name.
    fn from_domain(domain: String) -> Self {
        Self {
            domain,
            subdomain: None,
            ttl: 600,
        }
    }

    /// Checks if this job matches the given DNS record.
    pub fn check_record(&self, record: &DNSRecord) -> bool {
        match self.subdomain() {
            // '@' as a subdomain refers to the root of the domain; check the whole thing.
            Some("@") | None => record.name == self.domain,
            // Could do this by just just allocating "{subdomain}.{domain}" and comparing... but that means allocating!
            Some(sub) => {
                record.name.starts_with(sub)
                    && record.name.ends_with(&self.domain)
                    && record.name.len() == self.domain.len() + sub.len() + 1
                    && &record.name[sub.len()..sub.len() + 1] == "."
            },
        }
    }

    pub fn fmt_name(&self) -> String {
        match self.subdomain() {
            Some("@") | None => self.domain().to_string(),
            Some(sub) => format!("{sub}.{}", self.domain()),
        }
    }
}

/// [`DomainJob`] can be deserialized either as a single string or as a map of options.
impl<'de> Deserialize<'de> for DomainJob {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
        D::Error: de::Error,
    {
        // ----------------------------------------------------------------------------------------
        struct DomainVisitor;

        impl<'de> de::Visitor<'de> for DomainVisitor {
            type Value = DomainJob;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("string or map")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(DomainJob::from_domain(v.to_string()))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(DomainJob::from_domain(v))
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut domain = None;
                let mut subdomain = None;
                let mut ttl = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "domain" => domain = Some(map.next_value::<String>()?),
                        "subdomain" => subdomain = Some(map.next_value::<String>()?),
                        "ttl" => ttl = Some(map.next_value::<u32>()?),
                        other => return Err(de::Error::unknown_field(other, &["domain", "subdomain", "ttl"])),
                    }
                }

                let domain = domain.ok_or_else(|| de::Error::missing_field("domain"))?;
                let ttl = ttl.unwrap_or(600);

                Ok(DomainJob { domain, subdomain, ttl })
            }
        }
        // ----------------------------------------------------------------------------------------

        deserializer.deserialize_any(DomainVisitor)
    }
}
