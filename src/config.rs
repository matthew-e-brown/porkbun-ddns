use std::path::PathBuf;

use clap::Parser;
use eyre::WrapErr;
use serde::{Deserialize, Deserializer, de};
use tokio::fs;

use crate::api::model::DNSRecord;


// [FIXME] Serde does not support literals as default values yet: https://github.com/serde-rs/serde/issues/368
#[rustfmt::skip] const fn bool<const X: bool>() -> bool { X }
#[rustfmt::skip] const fn empty<T>() -> Vec<T> { Vec::new() }

// Internal struct for command-line flags: **not** the main program configuration. The main configuration comes from
// `Config`, which is loaded from a TOML file.
#[derive(Debug, clap::Parser)]
#[command(version, about, max_term_width = 100)]
struct Args {
    /// Path to TOML file containing configuration for the domains to update.
    #[arg(
        short = 'c',
        long = "cfg",
        env = "PORKBUN_DDNS_CONFIG",
        value_name = "FILE",
        default_value = "/etc/ddns.toml"
    )]
    cfg: PathBuf,

    /// Skip creating or modifying any DNS records on Porkbun.
    ///
    /// When this option is enabled, current IP addresses will be fetched and the records that need to be updated will
    /// be printed, but no changes will actually be made.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Update IPv4 (A) records for all domains.
    ///
    /// This command-line option force-enables IPv4 updates, regardless of what the 'ipv4' setting in the config file
    /// says.
    #[arg(long, conflicts_with = "no_ipv4")]
    ipv4: bool,

    /// Update IPv6 (AAAA) records for all domains.
    ///
    /// This command-line option force-enables IPv6 updates, regardless of what the 'ipv6' setting in the config file
    /// says.
    #[arg(long, conflicts_with = "no_ipv6")]
    ipv6: bool,

    /// Disable the updating of IPv4 (A) records for all domains.
    ///
    /// This command-line option force-disables IPv4 updates, regardless of what the 'ipv4' setting in the config file
    /// says.
    #[arg(long)]
    no_ipv4: bool,

    /// Disable the updating of IPv6 (AAAA) records for all domains.
    ///
    /// This command-line option force-disables IPv6 updates, regardless of what the 'ipv6' setting in the config file
    /// says.
    #[arg(long)]
    no_ipv6: bool,
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
    /// address when pinging Porkbun will not cause an error. When this option is true (and IPv6 is enabled), failure to
    /// acquire an IPv6 address will trigger a fatal error.
    #[serde(default = "bool::<false>")]
    pub ipv6_error: bool,

    /// A list of jobs describing domains/subdomains to update.
    #[serde(default = "empty")]
    pub targets: Vec<Target>,
}

impl Config {
    /// Loads runtime configuration from command line arguments and configuration file.
    pub async fn init() -> eyre::Result<Self> {
        let args = Args::parse();

        let text = fs::read_to_string(&args.cfg).await.wrap_err("failed to read config file")?;
        let mut config: Config = toml::from_str(&text).wrap_err("failed to parse config file")?;

        config.extend_from_args(&args);
        Ok(config)
    }

    /// Copies over non-TOML settings from the command line into this [`Config`] struct.
    fn extend_from_args(&mut self, args: &Args) {
        self.dry_run = args.dry_run;

        if args.ipv4 {
            self.ipv4 = true;
        } else if args.no_ipv4 {
            self.ipv4 = false;
        }

        if args.ipv6 {
            self.ipv6 = true;
        } else if args.no_ipv6 {
            self.ipv6 = false;
        }

        // ...other future settings.
    }
}


/// Specification for a single domain or subdomain to update.
#[derive(Debug)]
pub struct Target {
    domain: String,
    subdomain: Option<String>,
    ttl: u32,
}

impl Target {
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
    pub fn matches_record(&self, record: &DNSRecord) -> bool {
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

    /// Gets a print-friendly label for this target, representing how it was provided in the config file (e.g., this
    /// will return "@.domain.com" even though "@" is usually transparent)
    pub fn label(&self) -> String {
        match self.subdomain() {
            Some(sub) => format!("{sub}.{}", self.domain()),
            None => self.domain().to_string(),
        }
    }
}

/// A [`Target`] can be deserialized either as a single string or as a map of options.
impl<'de> Deserialize<'de> for Target {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
        D::Error: de::Error,
    {
        // ----------------------------------------------------------------------------------------
        struct DomainVisitor;

        impl<'de> de::Visitor<'de> for DomainVisitor {
            type Value = Target;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("string or map")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Target::from_domain(v.to_string()))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(Target::from_domain(v))
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

                if subdomain.as_deref() == Some("") {
                    subdomain = None;
                }

                Ok(Target { domain, subdomain, ttl })
            }
        }
        // ----------------------------------------------------------------------------------------

        deserializer.deserialize_any(DomainVisitor)
    }
}
