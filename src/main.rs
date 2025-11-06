mod api;
mod config;

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use futures::future;

use self::api::model::DNSRecord;
use self::api::{AddressType, IPv4, IPv6, PorkbunClient};
use self::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::init().await?;
    let porkbun = PorkbunClient::new()?;

    if !config.ipv4 && !config.ipv6 {
        anyhow::bail!("Both IPv4 and IPv6 are disabled: nothing to do.");
    }

    let (ipv4, ipv6) = get_addresses(&porkbun, &config).await?;

    // Get a unique list of root domain names (ignoring subdomains):
    let mut existing_records = BTreeMap::new();
    for job in &config.domains {
        // (Empty vector that will get replaced by actual vector)
        existing_records.entry(job.domain()).or_insert_with(Vec::<DNSRecord>::new);
    }

    // For each of those, fetch existing records:
    let tasks = existing_records.iter_mut().map(async |(domain, records)| {
        println!("Getting existing records for {domain}...");

        *records = porkbun.get_existing_records(domain).await?;
        records.retain(|res| res.typ == "A" || res.typ == "AAAA");

        anyhow::Ok(())
    });
    future::try_join_all(tasks).await?;

    // Now that we know our current IP addresses and we have a list of current records, actually go and do the updates
    let ipv4_fut = run_jobs::<IPv4>(&porkbun, &config, ipv4, &existing_records);
    let ipv6_fut = run_jobs::<IPv6>(&porkbun, &config, ipv6, &existing_records);
    future::try_join(ipv4_fut, ipv6_fut).await?;

    println!("Done!");
    Ok(())
}

/// Fetches both IPv4 and IPv6 addresses, as required by the given [`Config`], including console logging.
async fn get_addresses(
    porkbun: &PorkbunClient,
    config: &Config,
) -> anyhow::Result<(Option<Ipv4Addr>, Option<Ipv6Addr>)> {
    println!("Fetching current IP address...");

    let mut ipv4 = None;
    let mut ipv6 = None;

    // Ping the base `/ping` endpoint to see which type of IP address we get
    match porkbun.ping().await? {
        // If the non-IPv4 endpoint gives us IPv4, we don't have IPv6.
        IpAddr::V4(addr) => {
            if config.ipv4 {
                println!("Got IPv4 address: {addr}");
                ipv4 = Some(addr);
            }

            if config.ipv6 {
                println!("Got IPv6 address: None.");
                // Do the bail after printing the IPv4 so the user has more info to debug with
                if config.ipv6_error {
                    anyhow::bail!("Failed to retrieve IPv6 address from Porkbun");
                }
            }
        },
        IpAddr::V6(addr) => {
            if config.ipv6 {
                println!("Got IPv6 address: {addr}");
                ipv6 = Some(addr)
            }

            // Only now, if we got an IPv6 address but also want an IPv4 address, do we need the second ping.
            if config.ipv4 {
                let addr = porkbun.ping_v4().await?;
                println!("Got IPv4 address: {addr}");
                ipv4 = Some(addr);
            }
        },
    };

    Ok((ipv4, ipv6))
}

async fn run_jobs<A: AddressType>(
    porkbun: &PorkbunClient,
    config: &Config,
    addr: Option<A::Addr>,
    existing_records: &BTreeMap<&str, Vec<DNSRecord>>,
) -> anyhow::Result<()> {
    let Some(addr) = addr else {
        return Ok(());
    };

    let tasks = config.domains.iter().map(async |job| -> anyhow::Result<()> {
        let job_name = job.fmt_name();
        let existing = existing_records
            .get(job.domain())
            .and_then(|vec| vec.iter().find(|rec| job.check_record(rec)))
            .filter(|rec| rec.typ == A::RECORD_TYPE);

        if let Some(record) = existing {
            // Edit the record instead of creating a new one
            println!(
                "[{job_name}] Found existing '{}' record {} with content {}",
                A::RECORD_TYPE,
                record.id,
                record.content
            );

            if !config.dry_run {
                porkbun.edit_record::<A>(job, &record.id, &addr).await?;
                println!("[{job_name}] Edited record {} content from {} to {}", record.id, record.content, addr);
            } else {
                println!(
                    "[{job_name}] (DRY RUN) Skipped editing record {} content from {} to {}",
                    record.id, record.content, addr
                );
            }
        } else {
            println!("[{job_name}] No existing record found.");

            // Create a new record
            if !config.dry_run {
                porkbun.create_record::<A>(job, &addr).await?;
                println!("[{job_name}] New '{}' record created with content {}", A::RECORD_TYPE, addr);
            } else {
                println!("[{job_name}] (DRY RUN) Skipped creating '{}' record with content {}", A::RECORD_TYPE, addr);
            }
        }

        Ok(())
    });

    future::try_join_all(tasks).await?;
    Ok(())
}
