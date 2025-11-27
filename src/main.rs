mod api;
mod config;
mod logging;

use std::collections::HashMap;
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
                addresses = pluralize!("address", "addresses", app.config.num_enabled()),
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


/// The main application instance. Wraps both the program [`Config`] and a [`PorkbunClient`].
///
/// Having this be a separate struct alleviates needing to pass so many parameters around.
struct App {
    config: Config,
    client: PorkbunClient,
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
        Ok(Self { config, client })
    }

    /// Fetches IPv4 and IPv6 addresses for the current system.
    pub async fn get_addresses(&self) -> eyre::Result<(Option<Ipv4Addr>, Option<Ipv6Addr>)> {
        let num_enabled = self.config.num_enabled();
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
                if self.config.ipv4 {
                    log::info!("Found current IPv4 address: {addr}");
                    ipv4 = Some(addr);
                }

                if self.config.ipv6 {
                    // If IPv6 is set to hard-error mode, or if IPv6 is the only one enabled, this is an error.
                    if self.config.ipv6_error || !self.config.ipv4 {
                        return Err(eyre!("Tried to get IPv6 address from Porkbun API, but only got IPv4"));
                    }

                    // If the non-IPv4 endpoint gave us IPv4, then we can't get an IPv6. We don't even need to try.
                    log::info!("Found current IPv6 address: none.");
                }
            },
            IpAddr::V6(addr) => {
                if self.config.ipv6 {
                    log::info!("Found current IPv6 address: {addr}");
                    ipv6 = Some(addr);
                }

                if self.config.ipv4 {
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
        if self.config.dry_run {
            log::warn!("dry_run is enabled: no create/edit requests will be sent through to Porkbun.");
        }

        // Used for printing more accurate log messages.
        let search_type = match (ipv4, ipv6) {
            (Some(_), None) => "A",
            (None, Some(_)) => "AAAA",
            (Some(_), Some(_)) => "A/AAAA",
            // Shouldn't be possible to get this far with neither address; rest of application should have handled it.
            // Either way, this method has nothing to do, so we can just return as normal.
            (None, None) => return 0,
        };

        // Step 1: Fetch existing records for all domains
        // =============================================================================================================

        // First build a unique list of root domain names. Then we can send each one on its own task to get records.
        let mut current_records = HashMap::<&str, Box<[DNSRecord]>>::new();

        for target in &self.config.targets {
            // Start each one off with an empty (read: non-allocating) boxed-slice that can get replaced through a
            // `&mut` reference in each task.
            let domain = target.domain();
            current_records.entry(domain).or_insert_with(|| [].into());
        }

        log::debug!(
            "Checking Porkbun for existing DNS records on {} unique {domains}...",
            current_records.len(),
            domains = pluralize!("domain", "domains", current_records.len()),
        );

        let record_tasks = current_records.iter_mut().map(async |(domain, records)| -> Result<(), ()> {
            match self.client.get_existing_records(domain).await {
                Ok(mut existing) => {
                    // We only ever need A/AAAA records, get rid of everything else.
                    existing.retain(|r| (ipv4.is_some() && r.typ == "A") || (ipv6.is_some() && r.typ == "AAAA"));

                    let count = existing.len();
                    *records = existing.into_boxed_slice();

                    log::debug!(
                        "Found {count} {search_type} {records} for {domain}.",
                        records = pluralize!("record", "records", count)
                    );
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
            .config
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
        let mut matching_records = records.iter().filter(|rec| rec.typ == dns_type && target.matches_record(rec));

        let existing = matching_records.next();
        let num_left = matching_records.count();

        // There's probably some elegant way to handle multiple records existing for the same target. For now...
        // we'll let the user handle this.
        if num_left > 0 {
            return Err(eyre!("Multiple existing {dns_type} records matched target, unsure which to update"));
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
                if !self.config.dry_run {
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
            if !self.config.dry_run {
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
