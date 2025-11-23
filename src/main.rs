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
pub async fn main() -> eyre::Result<()> {
    let mut app = App::init().await.wrap_err("failed to initialize application")?;

    if !app.config.ipv4 && !app.config.ipv6 {
        println!("Nothing to do. Both IPv4 (A) and IPv6 (AAAA) are disabled.");
        return Ok(());
    }

    app.get_addresses().await?;
    app.get_records().await;
    app.run().await;
    Ok(())
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

struct App {
    config: Config,
    client: PorkbunClient,
    ipv4: Option<Ipv4Addr>,
    ipv6: Option<Ipv6Addr>,
    records: HashMap<String, Vec<DNSRecord>>,
    /*
    Possible solution?
    /// Tracks the total number of errors that have occurred over the runtime of the program.
    num_errors: Mutex<u8>,
    */
}

impl App {
    /// Initializes the application instance.
    pub async fn init() -> eyre::Result<Self> {
        let api_key = get_var("PORKBUN_API_KEY")?;
        let secret_key = get_var("PORKBUN_SECRET_KEY")?;

        let config = Config::init().await?;
        let client = PorkbunClient::new(api_key, secret_key);

        Ok(Self {
            config,
            client,
            ipv4: None,
            ipv6: None,
            // NB: Gets replaced by `get_records`, but that's okay: empty hashmap don't allocate.
            records: HashMap::new(),
        })
    }

    /// Gets the application's final return code based on errors that occurred during runtime.
    pub fn exit_code(&self) -> ExitCode {
        todo!();
    }

    fn report_error(error: eyre::Report) {
        todo!();
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
    /// Even though it is possible for record fetching to fail, this method is infallible. Instead of having each domain
    /// potentially throw and propagate errors, we let each one simply display its errors directly to the main output.
    /// Each target can then deal with the failure separately when it sees its list of records not present in the map.
    pub async fn get_records(&mut self) {
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
        let all_records = Mutex::new(&mut self.records);
        let rec_tasks = domains.into_iter().map(async |domain| -> () {
            println!("Fetching existing records for {domain}...");
            match self.client.get_existing_records(domain).await {
                Ok(mut records) => {
                    // We only ever need A/AAAA records, get rid of everything else.
                    records.retain(|r| (self.config.ipv4 && r.typ == "A") || (self.config.ipv6 && r.typ == "AAAA"));

                    println!("Found {} {search_type} records for {domain}.", records.len());

                    let mut map = all_records.lock().unwrap();
                    map.insert(domain.to_string(), records);
                },
                Err(err) => {
                    // [TODO] Report to user and propagate to process exit code
                    Err::<(), _>(err.wrap_err(format!("failed to fetch DNS records for domain {domain}"))).unwrap();
                },
            }
        });

        // Futures return `()` so this `JoinAll` future will never actually allocate a vector.
        futures::future::join_all(rec_tasks).await;
    }

    pub async fn run(&self) -> () {
        let tasks = self
            .config
            .targets
            .iter()
            .enumerate()
            .filter_map(|(i, target)| {
                let Some(records) = self.records.get(target.domain()) else {
                    // Target's records might be missing if we previously failed to fetch them. Error would've already
                    // been logged in that case, so we can just silently skip over this target.

                    // [TODO] Report to user, but don't propagate to process exit code (already done above).
                    Err::<(), _>(eyre!("target {i} skipped due to previous error")).unwrap();
                    return None;
                };

                let app = &*self; // Need a borrow that we can safely move into closure
                let addresses = [self.ipv4.map(IpAddr::V4), self.ipv6.map(IpAddr::V6)];
                let addr_tasks = addresses.into_iter().filter_map(move |addr| {
                    addr.map(async move |addr| {
                        if let Err(err) = app.handle_target(i, target, records, addr).await {
                            // [TODO] Report to user and propagate to process exit code
                            Err(err.wrap_err(format!("target {i} failed"))).unwrap()
                        }
                    })
                });

                Some(addr_tasks)
            })
            .flatten();

        futures::future::join_all(tasks).await;
    }

    async fn handle_target(&self, i: usize, target: &Target, records: &[DNSRecord], addr: IpAddr) -> eyre::Result<()> {
        // Check if any of the existing records for this target's domain actually match the target precisely:
        let mut matching_records = records
            .iter()
            .filter(|rec| rec.typ == addr.dns_type() && target.matches_record(rec));

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

    async fn do_nothing(&self, i: usize, target: &Target, record: &DNSRecord) -> eyre::Result<()> {
        let DNSRecord { typ, id, content, .. } = record;
        let label = target.label();
        let dr = if self.config.dry_run { " [DRY RUN]" } else { "" };
        println!("[{i}: {label}] Found existing {typ} record #{id} with content {content}.",);
        println!("[{i}: {label}]{dr} Nothing to do.");
        Ok(())
    }

    async fn edit_record(&self, i: usize, target: &Target, record: &DNSRecord, new_addr: IpAddr) -> eyre::Result<()> {
        let DNSRecord { typ, id, content, .. } = record;
        let label = target.label();
        println!("[{i}: {label}] Found existing {typ} record #{id} with content {content}.");

        let dr;
        if self.config.dry_run {
            dr = " [DRY RUN]";
        } else {
            dr = "";
            self.client
                .edit_record(target, &record.id, new_addr)
                .await
                .wrap_err("failed to edit DNS record")?;
        }

        println!("[{i}: {label}]{dr} Edited record #{id} content from {content} to {new_addr}.");
        Ok(())
    }

    async fn create_record(&self, i: usize, target: &Target, new_addr: IpAddr) -> eyre::Result<()> {
        let label = target.label();
        println!("[{i}: {label}] Did not find existing {} record.", new_addr.dns_type());

        let dr;
        let id;
        if self.config.dry_run {
            dr = " [DRY RUN]";
            id = "<record_id>".to_string();
        } else {
            dr = "";
            id = self
                .client
                .create_record(target, new_addr)
                .await
                .wrap_err("failed to create DNS record")?;
        }

        println!("[{i}: {label}]{dr} Created new record #{id} with content {new_addr}.");
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
