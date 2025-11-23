mod api;
mod config;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::ExitCode;
use std::sync::Mutex;

use eyre::{WrapErr, eyre};

use self::api::model::DNSRecord;
use self::api::{IpAddrExt, PorkbunClient};
use self::config::{Config, Target};

// - Read configuration and construct the client
// - Try and read the current IP addresses
//   - Error out completely if we couldn't get them
// - Grab all current A/AAAA records for all domain across all targets
//   - Store them as `Results` so we can do dependent jobs lazily
// - Once all records have been retrieved, targets can be done in parallel by looking up their records in the map

#[tokio::main]
pub async fn main() -> ExitCode {
    let mut app = match App::init().await.wrap_err("application failed to start") {
        Ok(app) => app,
        Err(err) => {
            report_error(err);
            return ExitCode::FAILURE;
        },
    };

    match app.get_addresses().await.wrap_err("failed to fetch IP address(es)") {
        Ok(()) => {},
        Err(err) => {
            report_error(err);
            return ExitCode::FAILURE;
        },
    }

    match app.run().await {
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

/// The main application instance. Wraps both
///
/// Having this be a separate struct alleviates the need for so many
struct App {
    config: Config,
    client: PorkbunClient,
    ipv4: Option<Ipv4Addr>,
    ipv6: Option<Ipv6Addr>,
    records: HashMap<String, Vec<DNSRecord>>,
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
        let config = Config::init().await?;

        let api_key = get_var("PORKBUN_API_KEY")?;
        let secret_key = get_var("PORKBUN_SECRET_KEY")?;
        let client = PorkbunClient::new(api_key, secret_key);

        Ok(Self {
            config,
            client,
            ipv4: None,
            ipv6: None,
            records: HashMap::new(),
        })
    }

    /// Fetches IPv4 and IPv6 addresses for the current system and stores them within the application instance.
    ///
    /// After completion, [`self.ipv4`] and [`self.ipv6`] will be set to `Some` if they were enabled in the
    /// configuration
    ///
    /// [`self.ipv4`]: Self::ipv4
    /// [`self.ipv6`]: Self::ipv6
    pub async fn get_addresses(&mut self) -> eyre::Result<()> {
        println!("Fetching IP addresses...");

        // Ping the base `/ping` endpoint first: it returns either IPv6 or IPv4.
        match self.client.ping().await? {
            IpAddr::V4(addr) => {
                if self.config.ipv4 {
                    println!("Got IPv4 address: {addr}");
                    self.ipv4 = Some(addr);
                } else {
                    self.ipv4 = None;
                }

                if self.config.ipv6 {
                    if self.config.ipv6_error {
                        return Err(eyre!("ipv6_error: could not get IPv6 address from Porkbun API"));
                    }

                    // If the non-IPv4 endpoint gave us IPv4, we can't get an IPv6 address, so we don't even need to
                    // try.
                    println!("Got IPv6 address: none.");
                    self.ipv6 = None;
                }
            },
            IpAddr::V6(addr) => {
                if self.config.ipv6 {
                    println!("Got IPv6 address: {addr}");
                    self.ipv6 = Some(addr);
                } else {
                    self.ipv6 = None;
                }

                if self.config.ipv4 {
                    println!("Pinging again for IPv4 address...");
                    let addr = self.client.ping_v4().await?;
                    println!("Got IPv4 address: {addr}");
                    self.ipv4 = Some(addr);
                } else {
                    self.ipv4 = None;
                }
            },
        }

        Ok(())
    }

    /// Fetch all currently existing DNS records for all target domains specified in the configuration.
    ///
    /// Even though it is possible for record fetching to fail, this method is infallible; instead, each failure is
    /// reported to the end user directly from within. This method then simply returns the number of errors that
    /// occurred while fetching. Any target whose domain failed to fetch records can then just log a simple message and
    /// skip running any further.
    async fn get_records(&mut self) -> usize {
        // First build a unique list of root domain names: then we can send each one on its own task to get records.
        let domains = {
            let mut v = Vec::new();
            // There should never really be that many targets/domains, so we'll just use a plain linear search to prune
            // duplicates. Intuition says this'll probably be cheaper overall than the potential overhead from something
            // like HashSet/BTreeSet. Plus, it lets us keep their relative order as defined in the config file.
            for target in &self.config.targets {
                let domain = target.domain();
                if !v.contains(&domain) {
                    v.push(domain);
                }
            }
            v
        };

        // Used for printing a more accurate log message:
        let search_type = match (self.config.ipv4, self.config.ipv6) {
            (true, false) => "A",
            (false, true) => "AAAA",
            _ => "A/AAAA",
        };

        // NB: std::mutex (blocking) is fine here over tokio::mutex (non-blocking) because we never need to hold the
        // lock past an await point. Either tokio is using separate threads for async runtime, and a blocking mutex is
        // fine; or it's using a single-threaded runtime, in which case no other thread could be holding the lock. This
        // would also make sense as a RwLock, except for the fact that we're only ever writing.
        let record_mutex = Mutex::new(&mut self.records);
        let record_tasks = domains.into_iter().map(async |domain| -> Result<(), ()> {
            println!("Fetching existing records for {domain}...");
            match self.client.get_existing_records(domain).await {
                Ok(mut records) => {
                    // We only ever need A/AAAA records, get rid of everything else.
                    records.retain(|r| (self.config.ipv4 && r.typ == "A") || (self.config.ipv6 && r.typ == "AAAA"));

                    println!("Found {count} {search_type} records for {domain}.", count = records.len());

                    let mut map = record_mutex.lock().expect("mutex should not be poisoned");
                    map.insert(domain.to_string(), records);
                    Ok(())
                },
                Err(err) => {
                    let err = err.wrap_err(format!("failed to fetch DNS records for domain {domain}"));
                    report_error(err);
                    Err(())
                },
            }
        });

        futures::future::join_all(record_tasks)
            .await
            .into_iter()
            .filter(Result::is_err)
            .count()
    }

    /// Run the application.
    ///
    /// Even though it is very possible for pieces of this application to fail
    pub async fn run(&mut self) -> usize {
        let mut err_count = self.get_records().await;

        let tasks = self
            .config
            .targets
            .iter()
            .enumerate()
            .filter_map(|(i, target)| {
                let i = i + 1; // 1-based indexing

                let Some(records) = self.records.get(target.domain()) else {
                    // Target's records might be missing if we previously failed to fetch them. Error would've already
                    // been logged in that case, so we don't need to report another one. Just a regular print is fine.
                    println!("target {i} ({}) skipped due to missing DNS records", target.label());

                    // Skip over this target in the outer `filter_map`.
                    return None;
                };

                // Convert an iterator of `Option<IpAddr>` into an iterator of `Option<impl Future>`, which gets
                // filtered down into an iterator of `impl Future`.
                let app = &*self; // Need non-mutable borrow to move into closure
                let addrs = [self.ipv4.map(IpAddr::V4), self.ipv6.map(IpAddr::V6)];
                let tasks = addrs.into_iter().filter_map(move |addr| {
                    addr.map(async move |addr: IpAddr| -> Result<(), ()> {
                        if let Err(err) = app.handle_target(i, target, records, addr).await {
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

        err_count += futures::future::join_all(tasks)
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
