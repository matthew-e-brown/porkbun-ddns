use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::{Debug, Display};
use std::path::PathBuf;

use eyre::{WrapErr, eyre};
use serde::de::DeserializeSeed;
use serde::{Deserialize, Deserializer, de};
use tokio::fs;

use crate::api::DNSRecord;


// [FIXME] Serde does not support literals as default values yet: https://github.com/serde-rs/serde/issues/368
#[rustfmt::skip] const fn bool<const X: bool>() -> bool { X }
#[rustfmt::skip] const fn empty<T>() -> Vec<T> { Vec::new() }

// Internal struct for command-line flags: **not** the main program configuration. The main configuration comes from
// `Config`, which is loaded from a TOML file.
#[derive(Debug, clap::Parser)]
#[command(version, about, max_term_width = 100)]
pub struct Args {
    /// Path to TOML file containing configuration for the domains to update.
    #[arg(
        short,
        long,
        env = "PORKBUN_DDNS_CONFIG",
        value_name = "FILE",
        default_value = "/etc/ddns.toml"
    )]
    pub config: PathBuf,

    /// Skip creating or modifying any DNS records on Porkbun.
    ///
    /// When this option is enabled, current IP addresses will be fetched and the records that need to be updated will
    /// be printed, but no changes will actually be made.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Controls the verbosity of logs.
    ///
    /// Possible log levels are 'error', 'warn', 'info', 'debug', and 'trace' (in that order).
    #[arg(long, env = "PORKBUN_LOG_LEVEL", value_name = "LEVEL", default_value = "info")]
    pub log_level: log::LevelFilter,

    /// Update IPv4 (A) records for all domains.
    ///
    /// This command-line option force-enables IPv4 updates, regardless of what the 'ipv4' setting in the config file
    /// says.
    #[arg(long, conflicts_with = "no_ipv4")]
    pub ipv4: bool,

    /// Update IPv6 (AAAA) records for all domains.
    ///
    /// This command-line option force-enables IPv6 updates, regardless of what the 'ipv6' setting in the config file
    /// says.
    #[arg(long, conflicts_with = "no_ipv6")]
    pub ipv6: bool,

    /// Disable the updating of IPv4 (A) records for all domains.
    ///
    /// This command-line option force-disables IPv4 updates, regardless of what the 'ipv4' setting in the config file
    /// says.
    #[arg(long)]
    pub no_ipv4: bool,

    /// Disable the updating of IPv6 (AAAA) records for all domains.
    ///
    /// This command-line option force-disables IPv6 updates, regardless of what the 'ipv6' setting in the config file
    /// says.
    #[arg(long)]
    pub no_ipv6: bool,
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
    pub async fn from_args(args: Args) -> eyre::Result<Self> {
        if log::log_enabled!(log::Level::Trace) {
            log::trace!("Reading configuration from {}", &args.config.to_string_lossy());
        }

        let text = fs::read_to_string(&args.config).await.wrap_err("Failed to read config file")?;
        let mut config: Config = toml::from_str(&text).wrap_err("Failed to parse config file")?;

        config.extend_from_args(&args);

        // Again, this will run in a cron job / timer. This is a lot of unnecessary stuff to dump into logs.
        // It may be helpful to have again later, though...
        /* log::trace!("Final config: {config:?}"); */

        // Check that all targets are unique:
        let mut tgt_labels = HashMap::with_capacity(config.targets.len());
        let mut i = 0;
        for tgt in &config.targets {
            i += 1;
            match tgt_labels.entry(tgt.to_string()) {
                Entry::Vacant(entry) => {
                    entry.insert(i);
                },
                Entry::Occupied(entry) => {
                    let j = *entry.get();
                    let k = entry.key();
                    return Err(eyre!("Target {k} specified more than once (targets {j} and {i})")
                        .wrap_err("Invalid configuration"));
                },
            }
        }

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
#[derive(Debug, Clone)]
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

    /// Creates a default [`Target`] out of just a domain name.
    fn from_domain(domain: String) -> Self {
        Self {
            domain,
            subdomain: None,
            ttl: 600,
        }
    }

    /// Checks if the given [record][DNSRecord] matches this [target][Target].
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
}

/// Formats a [`Target`] as a single domain name that represents how it was specified in the config file.
///
/// For example, domains specified with a subdomain of `@` will be printed as `@.example.com`, even though the actual
/// name that would get sent to Porkbun would just be `example.com`.
impl Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(sub) = self.subdomain() {
            write!(f, "{sub}.")?;
        }
        write!(f, "{}", self.domain())
    }
}

/// A [`Target`] can be deserialized either as a single string or as a map of options.
impl<'de> Deserialize<'de> for Target {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
        D::Error: de::Error,
    {
        deserializer.deserialize_any(TargetVisitor)
    }
}

struct TargetVisitor;

impl<'de> de::Visitor<'de> for TargetVisitor {
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

        while let Some(key) = map.next_key::<Box<str>>()? {
            match &key[..] {
                "domain" => domain = Some(map.next_value_seed(DomainSegment::DOMAIN)?),
                "subdomain" => subdomain = Some(map.next_value_seed(DomainSegment::SUBDOMAIN)?),
                "ttl" => ttl = Some(map.next_value::<u32>()?),
                other => return Err(de::Error::unknown_field(other, &["domain", "subdomain", "ttl"])),
            }
        }

        let domain = domain.ok_or_else(|| de::Error::missing_field("domain"))?;
        let subdomain = subdomain.filter(|str| &str[..] != "");
        let ttl = ttl.unwrap_or(600);

        Ok(Target { domain, subdomain, ttl })
    }
}

/// A [`DeserializeSeed`] impl. that deserializes a string while enforcing that it does not contain whitespace. The
/// seeded version of `Deserialize` is used simply to allow for a better error message.
struct DomainSegment(&'static str);

impl DomainSegment {
    pub const DOMAIN: DomainSegment = DomainSegment("domain names");
    pub const SUBDOMAIN: DomainSegment = DomainSegment("subdomains");
}

impl<'de> DeserializeSeed<'de> for DomainSegment {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        let str = String::deserialize(deserializer)?;
        match str.chars().find(|c| c.is_whitespace()) {
            Some(_) => Err(de::Error::custom(format_args!("{} may not contain whitespace", self.0))),
            None => Ok(str),
        }
    }
}
