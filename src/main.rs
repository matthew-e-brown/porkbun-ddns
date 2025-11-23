mod api;
mod config;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::ExitCode;

use eyre::{WrapErr, eyre};

use self::api::model::DNSRecord;
use self::api::{IpAddrExt, PorkbunClient};
use self::config::{Config, Target};

#[tokio::main]
pub async fn main() -> ExitCode {
    let app = match App::init().await.wrap_err("application failed to start") {
        Ok(app) => app,
        Err(err) => {
            report_error(err);
            return ExitCode::FAILURE;
        },
    };

    let (ipv4, ipv6) = match app.get_addresses().await.wrap_err("failed to fetch IP address(es)") {
        // `get_addresses` will return two `None`s only if both are disabled. Otherwise, at least one is enabled,
        // meaning the only other option is for an error to have occurred or for at least one of them to be valid.
        Ok((None, None)) => {
            println!("Both IPv4 and IPv6 are disabled. Nothing to do.");
            return ExitCode::SUCCESS;
        },
        Ok(addrs) => addrs,
        Err(err) => {
            report_error(err);
            return ExitCode::FAILURE;
        },
    };

    match app.run(ipv4, ipv6).await {
        0 => {
            println!("Done.");
            ExitCode::SUCCESS
        },
        n => {
            eprintln!(
                "Encountered {n} {errors}. See output for details.",
                errors = if n == 1 { "error" } else { "errors" },
            );

            ExitCode::FAILURE
        },
    }
}

fn report_error(err: eyre::Report) {
    todo!("error report: {err}");
}

/// The main application instance. Wraps both the program [`Config`] and a [`PorkbunClient`].
///
/// Having this be a separate struct alleviates needing to pass so many parameters around.
struct App {
    config: Config,
    client: PorkbunClient,
}

#[inline]
#[cfg(feature = "dotenv")]
fn get_var(key: &str) -> eyre::Result<String> {
    dotenvy::var(key).wrap_err_with(|| format!("failed to load environment variable {key}"))
}

#[inline]
#[cfg(not(feature = "dotenv"))]
fn get_var(key: &str) -> eyre::Result<String> {
    std::env::var(key).wrap_err_with(|| format!("failed to load environment variable {key}"))
}

impl App {
    /// Initializes the application instance.
    pub async fn init() -> eyre::Result<Self> {
        let config = Config::load().await.wrap_err("failed to load config")?;

        let api_key = get_var("PORKBUN_API_KEY")?;
        let secret_key = get_var("PORKBUN_SECRET_KEY")?;
        let client = PorkbunClient::new(api_key, secret_key);

        Ok(Self { config, client })
    }

    /// Fetches IPv4 and IPv6 addresses for the current system.
    pub async fn get_addresses(&self) -> eyre::Result<(Option<Ipv4Addr>, Option<Ipv6Addr>)> {
        let num_enabled = self.config.ipv4 as usize + self.config.ipv6 as usize;
        println!(
            "Fetching current IP {addresses}...",
            addresses = if num_enabled == 1 { "address" } else { "addresses" },
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
                    println!("Got IPv4 address: {addr}");
                    ipv4 = Some(addr);
                }

                if self.config.ipv6 {
                    // If IPv6 is set to hard-error mode, or if IPv6 is the only one enabled, this is an error.
                    if self.config.ipv6_error || !self.config.ipv4 {
                        return Err(eyre!("failed to get IPv6 address from Porkbun API"));
                    }

                    // If the non-IPv4 endpoint gave us IPv4, then we can't get an IPv6. We don't even need to try.
                    println!("Got IPv6 address: none.");
                }
            },
            IpAddr::V6(addr) => {
                if self.config.ipv6 {
                    println!("Got IPv6 address: {addr}");
                    ipv6 = Some(addr);
                }

                if self.config.ipv4 {
                    println!("Pinging again for IPv4 address...");
                    let addr = self.client.ping_v4().await?;
                    println!("Got IPv4 address: {addr}");
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

        let record_tasks = current_records.iter_mut().map(async |(domain, records)| -> Result<(), ()> {
            match self.client.get_existing_records(domain).await {
                Ok(mut existing) => {
                    // We only ever need A/AAAA records, get rid of everything else.
                    existing.retain(|r| (ipv4.is_some() && r.typ == "A") || (ipv6.is_some() && r.typ == "AAAA"));

                    let count = existing.len();
                    *records = existing.into_boxed_slice();

                    println!("Found {count} {search_type} records for {domain}.");
                    Ok(())
                },
                Err(err) => {
                    let err = err.wrap_err(format!("failed to fetch DNS records for domain {domain}"));
                    report_error(err);
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
            .enumerate()
            .filter_map(|(i, target)| {
                let i = i + 1; // 1-based indexing

                let Some(records) = current_records.get(target.domain()) else {
                    // Target's records might be missing if we previously failed to fetch them. Error would've already
                    // been logged in that case, so we don't need to report another one. Just a regular print is fine.
                    println!("target {i} ({}) skipped due to missing DNS records", target.label());

                    // Skip over this target in the outer `filter_map`.
                    return None;
                };

                // Convert an iterator of `Option<IpAddr>` into an iterator of `Option<impl Future>`, which gets
                // filtered down into an iterator of `impl Future`.
                let addrs = [ipv4.map(IpAddr::V4), ipv6.map(IpAddr::V6)];
                let tasks = addrs.into_iter().filter_map(move |addr| {
                    addr.map(async move |addr| -> Result<(), ()> {
                        if let Err(err) = self.handle_target(i, target, records, addr).await {
                            let msg = format!("target {i} ({}) failed", target.label());
                            report_error(err.wrap_err(msg));
                            Err(())
                        } else {
                            Ok(())
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

    async fn handle_target(&self, i: usize, target: &Target, records: &[DNSRecord], addr: IpAddr) -> eyre::Result<()> {
        // Check if any of the existing records for this target's domain actually match the target precisely:
        let mut matching_records = records
            .iter()
            .filter(|rec| rec.typ == addr.dns_type() && target.record_matches(rec));

        let existing = matching_records.next();
        let num_left = matching_records.count();

        // There's probably some elegant way to handle multiple records existing for the same target. For now...
        // we'll let the user handle this.
        if num_left > 0 {
            return Err(eyre!("multiple existing DNS records match target, unsure which to update"));
        }

        match existing {
            // If the address on the record matches our current address, we don't need to update anything.
            Some(record) if ip_matches(record, addr)? => self.do_nothing(i, target, record).await,
            // If there is otherwise an existing record, we need to edit it.
            Some(record) => self.edit_record(i, target, record, addr).await,
            // Otherwise, we need to create a new one.
            None => self.create_record(i, target, addr).await,
        }
    }

    // [TODO] Tidy the whole "reporting" situation up. Sort of in-between rewrites here, so:
    //
    // - Currently there is both `target.label()` for just getting the domain name, plus this function with extra
    //   formatting on main log lines
    // - Reported errors use a different formatting for targets than regular log output.
    //
    // We'll have to deal with this messiness later.

    /// Gets a print-friendly label for the given target, representing how it was provided in the config file (e.g.,
    /// this will return "@.domain.com" even though "@" is usually transparent). Used for log output lines.
    fn fmt_label(&self, i: usize, target: &Target) -> String {
        let mut label = match target.subdomain() {
            Some(sub) => format!("[{i}: {sub}.{}]", target.domain()),
            None => format!("[{i}: {}]", target.domain()),
        };

        if self.config.dry_run {
            label += " [DRY RUN]";
        }

        label
    }

    async fn do_nothing(&self, i: usize, target: &Target, record: &DNSRecord) -> eyre::Result<()> {
        let DNSRecord { typ, id, content, .. } = record;
        let label = self.fmt_label(i, target);
        println!("{label} Found existing {typ} record #{id} with content {content}.",);
        println!("{label} Nothing to do.");
        Ok(())
    }

    async fn edit_record(&self, i: usize, target: &Target, record: &DNSRecord, new_addr: IpAddr) -> eyre::Result<()> {
        let DNSRecord { typ, id, content, .. } = record;
        let label = self.fmt_label(i, target);
        println!("{label} Found existing {typ} record #{id} with content {content}.");

        if !self.config.dry_run {
            self.client
                .edit_record(target, &record.id, new_addr)
                .await
                .wrap_err("failed to edit DNS record")?;
        }

        println!("{label} Edited record #{id} content from {content} to {new_addr}.");
        Ok(())
    }

    async fn create_record(&self, i: usize, target: &Target, new_addr: IpAddr) -> eyre::Result<()> {
        let label = self.fmt_label(i, target);
        println!("{label} Did not find existing {} record.", new_addr.dns_type());

        let id;
        if !self.config.dry_run {
            id = self
                .client
                .create_record(target, new_addr)
                .await
                .wrap_err("failed to create DNS record")?;
        } else {
            id = "<record_id>".to_string();
        }

        println!("{label} Created new record #{id} with content {new_addr}.");
        Ok(())
    }
}

/// Checks if the actual content of the given record matches the current IP address.
fn ip_matches(record: &DNSRecord, current_addr: IpAddr) -> eyre::Result<bool> {
    let record_addr = record
        .content
        .parse::<IpAddr>()
        .wrap_err("existing DNS record has invalid IP address")?;
    if record.typ != record_addr.dns_type() {
        Err(eyre!("existing DNS record's IP address type does not match its record type"))
    } else {
        Ok(current_addr == record_addr)
    }
}
