mod api;
mod config;
mod logging;

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::ExitCode;

use eyre::{WrapErr, eyre};

use self::api::{DNSRecord, IpAddrExt, PorkbunClient};
use self::config::{Config, Target};
use self::logging::Logger;

/// Formatting helper for log and error messages
macro_rules! pluralize {
    ($single:expr, $plural:expr, $count:expr) => {
        if $count == 1 { $single } else { $plural }
    };
}

#[tokio::main]
pub async fn main() -> ExitCode {
    Logger::new().init().expect("no other logger should have been set yet");

    let app = match App::init().await {
        Ok(app) => app,
        Err(err) => {
            log::error!("{err:#}");
            return ExitCode::FAILURE;
        },
    };

    let (ipv4, ipv6) = match app.get_addresses().await {
        // `get_addresses` will return two `None`s only if both are disabled. Otherwise, at least one is enabled,
        // meaning the only other option is for an error to have occurred or for at least one of them to be valid.
        Ok((None, None)) => {
            log::info!("Both IPv4 and IPv6 are disabled. Nothing to do.");
            return ExitCode::SUCCESS;
        },
        Ok(addrs) => addrs,
        Err(err) => {
            log::error!(
                "Failed to determine current IP {addresses}: {err:#}",
                addresses = pluralize!("address", "addresses", app.mode_count()),
            );
            return ExitCode::FAILURE;
        },
    };

    match app.run(ipv4, ipv6).await {
        0 => {
            log::info!("Done.");
            ExitCode::SUCCESS
        },
        n => {
            log::error!("Encountered {n} {errors}. See output for details.", errors = pluralize!("error", "errors", n));
            ExitCode::FAILURE
        },
    }
}

/// Gets a variable from the environment or from a `.env` file.
#[inline]
#[cfg(feature = "dotenv")]
fn get_var(key: &str) -> Result<String, dotenvy::Error> {
    dotenvy::var(key)
}

/// Gets a variable from the environment.
#[inline]
#[cfg(not(feature = "dotenv"))]
fn get_var(key: &str) -> Result<String, std::env::VarError> {
    std::env::var(key)
}

/// The main application instance.
///
/// Having this be a separate struct alleviates needing to pass so many parameters around.
struct App {
    client: PorkbunClient,
    ipv4_enabled: bool,
    ipv6_enabled: bool,
    ipv6_error: bool,
    dry_run: bool,
    targets: Vec<Target>,
}

impl App {
    /// Returns the number of IP address modes (IPv4, IPv6) that are enabled (0, 1, or 2).
    pub const fn mode_count(&self) -> usize {
        self.ipv4_enabled as usize + self.ipv6_enabled as usize
    }
}

impl App {
    /// Initializes the application instance.
    pub async fn init() -> eyre::Result<Self> {
        log::debug!("Initializing...");

        let config = Config::load().await?;

        log::trace!("Loading API keys from environment");
        let api_key = get_var("PORKBUN_API_KEY").wrap_err("Failed to get PORKBUN_API_KEY from environment")?;
        let secret_key = get_var("PORKBUN_SECRET_KEY").wrap_err("Failed to get PORKBUN_SECRET_KEY from environment")?;
        let client = PorkbunClient::new(api_key, secret_key);

        log::debug!("Initialization successful.");
        Ok(App {
            client,
            ipv4_enabled: config.ipv4,
            ipv6_enabled: config.ipv6,
            ipv6_error: config.ipv6_error,
            dry_run: config.dry_run,
            targets: config.targets,
        })
    }

    /// Fetches IPv4 and IPv6 addresses for the current system.
    pub async fn get_addresses(&self) -> eyre::Result<(Option<Ipv4Addr>, Option<Ipv6Addr>)> {
        let num_enabled = self.mode_count();
        log::debug!(
            "Pinging Porkbun API for current IP {addresses}...",
            addresses = pluralize!("address", "addresses", num_enabled),
        );

        if num_enabled == 0 {
            return Ok((None, None));
        }

        let mut ipv4 = None;
        let mut ipv6 = None;

        // Ping the base `/ping` endpoint first: it returns either IPv6 or IPv4.
        match self.client.ping().await? {
            IpAddr::V4(addr) => {
                if self.ipv4_enabled {
                    log::info!("Found current IPv4 address: {addr}");
                    ipv4 = Some(addr);
                }

                if self.ipv6_enabled {
                    // If IPv6 is set to hard-error mode, or if IPv6 is the only one enabled, this is an error.
                    if self.ipv6_error || !self.ipv4_enabled {
                        return Err(eyre!("Tried to get IPv6 address from Porkbun API, but only got IPv4"));
                    }

                    // If the non-IPv4 endpoint gave us IPv4, then we can't get an IPv6. We don't even need to try.
                    log::info!("Found current IPv6 address: none.");
                }
            },
            IpAddr::V6(addr) => {
                if self.ipv6_enabled {
                    log::info!("Found current IPv6 address: {addr}");
                    ipv6 = Some(addr);
                }

                if self.ipv4_enabled {
                    log::debug!("Pinging again for IPv4 address...");
                    let addr = self.client.ping_v4().await?;
                    log::info!("Found current IPv4 address: {addr}");
                    ipv4 = Some(addr);
                }
            },
        }

        Ok((ipv4, ipv6))
    }

    /// Run the application.
    ///
    /// Even though it is very possible for pieces of this application to fail, this method does not return a `Result`.
    /// Instead, this method handles logging/reporting all errors that occur over the course of the entire operation.
    /// Then, the total number of errors is returned.
    pub async fn run(&self, ipv4: Option<Ipv4Addr>, ipv6: Option<Ipv6Addr>) -> usize {
        if self.dry_run {
            log::warn!("dry_run is enabled: no create/edit requests will be sent through to Porkbun.");
        }

        // Step 1: Fetch existing records for all domains
        // =============================================================================================================

        // First build a unique list of root domain names. Then we can send each one on its own task to get records.
        let mut current_records = HashMap::<&str, Vec<DNSRecord>>::new();

        for target in &self.targets {
            // Start each one off with an empty (read: non-allocating) vec that can get extended by each task.
            let domain = target.domain();
            current_records.entry(domain).or_insert_with(Vec::new);
        }

        log::debug!(
            "Querying Porkbun API for {n} {domains} existing DNS records...",
            n = current_records.len(),
            domains = pluralize!("domain's", "domains'", current_records.len()),
        );

        let record_tasks = current_records.iter_mut().map(async |(domain, records)| -> Result<(), ()> {
            match self.client.get_existing_records(domain).await {
                Ok(existing) => {
                    *records = existing;

                    if log::log_enabled!(log::Level::Debug) {
                        log_records(log::Level::Debug, domain, records);
                    }

                    Ok(())
                },
                Err(err) => {
                    log::error!("Failed to fetch DNS records for {domain}: {err:#}");
                    Err(())
                },
            }
        });

        let mut err_count = futures::future::join_all(record_tasks)
            .await
            .into_iter()
            .filter(Result::is_err)
            .count();

        // Step 2: Actually process all of the targets
        // =============================================================================================================

        let target_tasks = self
            .targets
            .iter()
            .filter_map(|target| {
                let Some(records) = current_records.get(target.domain()) else {
                    // Target's records might be missing if we previously failed to fetch them. Error would've already
                    // been logged in that case, so we don't need to report another one.
                    log::warn!("{target}: Skipped due to missing DNS records.");
                    // Skip over this target in the outer `filter_map`.
                    return None;
                };

                // Convert an iterator of `Option<IpAddr>` into an iterator of `Option<impl Future>`, which gets
                // filtered down into an iterator of `impl Future`.
                let addrs = [ipv4.map(IpAddr::V4), ipv6.map(IpAddr::V6)];
                let tasks = addrs.into_iter().filter_map(move |addr| {
                    addr.map(async move |addr| -> Result<(), ()> {
                        match self.handle_target(target, records, addr).await {
                            Ok(()) => Ok(()),
                            Err(err) => {
                                log::error!("{target}: {err:#}");
                                Err(())
                            },
                        }
                    })
                });

                // Return an `Iterator<impl Future>` to the outer `filter_map`, giving `Iter<Iter<impl Future>>`, which
                // then gets flattened down into one final iterator of futures.
                Some(tasks)
            })
            .flatten();

        err_count += futures::future::join_all(target_tasks)
            .await
            .into_iter()
            .filter(Result::is_err)
            .count();

        err_count
    }

    async fn handle_target<'a>(&self, target: &Target, records: &'a [DNSRecord], addr: IpAddr) -> eyre::Result<()> {
        let dns_type = addr.dns_type();

        // Check if any of the existing records for this target's domain actually match the target precisely:
        let mut existing = None;
        for record in records {
            if !target.matches_record(record) {
                continue;
            }

            if record.typ == dns_type {
                if existing.is_none() {
                    existing = Some(record);
                } else {
                    // We don't really have a way to handle when there are multiple existing records. Do we replace both
                    // of them? How can we know if that's a good idea if we don't know why there are two? We'll just let
                    // the user deal with it (for now, at least).
                    return Err(eyre!(
                        "Found more than one existing {dns_type} records for {target}, unsure which to update"
                    ));
                }
            } else if record.typ == "CNAME" || record.typ == "ALIAS" {
                // It's not possible to create an A or AAAA record when there is an ALIAS or a CNAME record, since those
                // work by passing records through to another host. Porkbun's API ideally should handle this and return
                // an error in their API response, but the message they return doesn't actually give a reason (it does
                // in their web interface, though). So, we'll keep an eye out for it.
                return Err(eyre!("A CNAME or ALIAS record already exists for host {target}")
                    .wrap_err(format!("Can't create {dns_type} record")));
            }
        }

        if let Some(record) = existing {
            let id = &record.id[..];

            // Check what the IP address is on the existing record
            let existing_addr = record
                .try_parse_ip()
                .wrap_err_with(|| format!("Found matching {dns_type} record #{id}, but it was malformed"))?;

            // If the address on the record matches our current address, we don't need to update anything.
            if existing_addr == addr {
                log::info!("{target}: Nothing to do. Found existing {dns_type} record #{id} with content {addr}.");
                Ok(())
            } else {
                if !self.dry_run {
                    self.client
                        .edit_record(target, id, addr)
                        .await
                        .wrap_err("Failed to edit DNS record")?;
                }

                log::info!("{target}: Edited existing {dns_type} record #{id} from {existing_addr} to {addr}.");
                Ok(())
            }
        } else {
            let id;
            if !self.dry_run {
                id = self
                    .client
                    .create_record(target, addr)
                    .await
                    .wrap_err("Failed to create DNS record")?;
            } else {
                id = "<record_id>".to_string();
            }

            log::info!("{target}: Created new {dns_type} record #{id} with content {addr}.");
            Ok(())
        }
    }
}

/// Helper function for logging which records were retrieved for a given domain.
fn log_records(level: log::Level, domain: &str, records: &[DNSRecord]) {
    if records.len() == 0 {
        log::log!(level, "Found 0 existing records for {domain}.");
    } else {
        // Count how many records of each specific type we found:
        let mut counts = BTreeMap::new();
        for rec in &*records {
            *(counts.entry(&rec.typ[..]).or_insert(0usize)) += 1;
        }

        // Don't feel like bringing all of itertools in just to get `.join`...
        let mut counts = counts.into_iter();
        let counts_str = counts
            .next()
            .map(move |(typ, n)| {
                counts.fold(format!("{n} {typ}"), |mut acc, (typ, n)| {
                    let bits = [", ", &n.to_string(), " ", typ];
                    acc.reserve(bits.into_iter().map(str::len).sum());
                    acc.extend(bits);
                    acc
                })
            })
            .unwrap(); // We already know there is at least one record

        log::log!(
            level,
            "Found {count} total {records} for {domain} ({counts_str}).",
            count = records.len(),
            records = pluralize!("record", "records", records.len())
        );
    }
}
