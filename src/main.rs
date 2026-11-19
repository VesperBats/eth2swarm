mod accumulator;
mod addr;
mod chain;
mod client;
mod devnet;
mod import;
mod inspect;
mod layout;
mod swarm;

use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::http::reqwest::Url;
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use swarm::bee::Bee;

/// ethswarm — import Ethereum blocks into Ethereum Swarm.
#[derive(Parser)]
#[command(name = "ethswarm", version, about)]
struct Cli {
    /// JSON-RPC endpoint of the execution client.
    /// Defaults to $ETH2SWARM_RPC, then the zbox mainnet node
    /// (use http://localhost:8545 for the bee-factory Anvil).
    #[arg(
        long,
        global = true,
        env = "ETH2SWARM_RPC",
        default_value = "http://zbox.tail2ab6c8.ts.net:8561"
    )]
    rpc: Url,

    /// Bee API endpoint (bee-factory: worker 1 holds the postage batch).
    #[arg(
        long,
        global = true,
        env = "ETH2SWARM_BEE",
        default_value = "http://localhost:1635"
    )]
    bee: Url,

    #[command(subcommand)]
    command: Command,
}

/// Where a client finds data on Swarm.
#[derive(clap::Args)]
struct Source {
    /// Address of the SOC owner (the importer's key).
    #[arg(long, env = "ETH2SWARM_OWNER")]
    owner: Address,
    /// Chain ID used in the SOC identifiers (1 = Ethereum mainnet; 1337 = bee-factory Anvil).
    #[arg(long, env = "ETH2SWARM_CHAIN_ID", default_value_t = 1)]
    chain_id: u64,
    /// Pinned trust anchor. If omitted, the owner's published anchor is used (trusts the
    /// publisher instead of verifying independently).
    #[arg(long, env = "ETH2SWARM_ANCHOR")]
    anchor: Option<B256>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the connected chain and its current head.
    Tip,
    /// Print a summary of a single block.
    Block {
        /// Block number.
        number: u64,
    },
    /// Publish the address index to Swarm (docs/address-index.md): a backfill on the first
    /// run, then one slice and one head update per finalized unit, resuming from --state.
    AddrImport {
        /// Private key of the SOC owner.
        #[arg(long, env = "ETH2SWARM_KEY", hide_env_values = true)]
        key: PrivateKeySigner,
        /// Directory for the importer's state file.
        #[arg(long)]
        state: std::path::PathBuf,
        /// Import up to this many units (default: every finalized unit).
        #[arg(long)]
        units: Option<u64>,
        /// Keep importing newly finalized units until stopped. Without it, a run returns once
        /// the target is published and the merges it started are built.
        #[arg(long)]
        follow: bool,
        /// Start a new index (first run, no state yet). Refused if the key already has one (for
        /// an export, --out, that is checked by addr-upload before it sends anything).
        #[arg(long)]
        init: bool,
        /// With --init: the index covers the 64-block unit of this block and everything after
        /// it, instead of the whole chain. Lookups report the blocks before it as not covered.
        /// The start is fixed for the life of the index (an earlier start needs another key).
        #[arg(long)]
        from_block: Option<u64>,
        /// Also read `trace_block` (see `appearances`).
        #[arg(long)]
        traces: bool,
        /// Postage batch ID for the permanent records (everything, without --lower-batch).
        /// Defaults to the Bee node's first usable batch.
        #[arg(long)]
        batch: Option<B256>,
        /// A batch for the lower slices, which die once the head has passed them (repeatable).
        /// Each lower slice goes to the one that expires soonest after the slice is dead plus
        /// --grace-days, or to --batch if none lives that long. Heads, the origin and the
        /// top-level slices always go to --batch. The importer never buys or tops up batches.
        #[arg(long = "lower-batch")]
        lower_batches: Vec<B256>,
        /// Days a lower batch must outlive a slice (default 7): for clients holding an older
        /// head, a deferred merge still being built (whose expansion stays in use meanwhile),
        /// and the importer being stopped (its head's slices stay in use).
        #[arg(long)]
        grace_days: Option<u64>,
        /// Write everything to this directory instead of uploading it (no Bee node needed);
        /// `addr-upload` pushes it through a Bee node later. Every run of the index then
        /// passes the same directory. Keep it apart from --state.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Merges of this level and above are built in the background.
        #[arg(long, default_value_t = 6)]
        defer_level: u8,
        /// Highest slice level built. Memory is bounded by --sort-memory-mb except for the
        /// busiest address of a slice, whose entries and spill file are held whole: up to
        /// level 12 (about a month) that stays in the hundreds of MB; above it a token
        /// contract can need GBs and a spill file beyond what clients read (still open).
        #[arg(long, default_value_t = 12)]
        max_level: u8,
        /// Directory for the sort runs of the slice being built (default: --state); they go in
        /// an `ethswarm-sort` directory of their own there. About 45 bytes per appearance of
        /// the largest slice, removed once it is built.
        #[arg(long)]
        sort_dir: Option<std::path::PathBuf>,
        /// Memory for one sort buffer, in MiB. A background merge and the newest unit can each
        /// hold one.
        #[arg(long, default_value_t = 512, value_parser = clap::value_parser!(u64).range(1..=1 << 20))]
        sort_memory_mb: u64,
        /// Blocks fetched, and chunks uploaded, in parallel.
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
    },
    /// Dev chains only: send traffic that exercises every kind of appearance (ETH transfers,
    /// a contract, `Transfer` logs, a busy address that spills, an EIP-7702 authorization).
    DevnetActivity {
        /// Key that sends the transactions (a funded dev account).
        #[arg(long)]
        key: PrivateKeySigner,
        /// Key that signs the EIP-7702 authorization (another dev account).
        #[arg(long)]
        authority_key: PrivateKeySigner,
        /// The address that receives every transfer.
        #[arg(long)]
        busy: Address,
        /// Number of `Transfer` calls.
        #[arg(long, default_value_t = 120)]
        transfers: usize,
        /// Allow this chain ID besides the dev chains 1337 and 31337 (the command refuses
        /// any other chain).
        #[arg(long)]
        allow_chain_id: Option<u64>,
    },
    /// Upload an address-index export (`addr-import --out`) through the Bee node, in the
    /// order the importer wrote it, checking every file and SOC first. Resumes where the last
    /// upload of the directory stopped.
    AddrUpload {
        /// The export directory.
        dir: std::path::PathBuf,
        /// Postage batch ID for the permanent records (everything, without --lower-batch).
        /// Defaults to the batch of the earlier uploads of this directory, else the Bee node's
        /// first usable batch.
        #[arg(long)]
        batch: Option<B256>,
        /// A batch for the lower slices, which die once the head has passed them (repeatable).
        /// Each lower slice goes to the one that expires soonest after the slice is dead plus
        /// --grace-days, or to --batch if none lives that long. Heads, the origin and the
        /// top-level slices always go to --batch. The importer never buys or tops up batches.
        #[arg(long = "lower-batch")]
        lower_batches: Vec<B256>,
        /// Days a lower batch must outlive a slice (default 7): for clients holding an older
        /// head, a deferred merge still being built (whose expansion stays in use meanwhile),
        /// and the importer being stopped (its head's slices stay in use).
        #[arg(long)]
        grace_days: Option<u64>,
        /// Records uploaded in parallel (between two heads).
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
        /// Upload everything again, ignoring the progress of earlier uploads.
        #[arg(long)]
        from_start: bool,
    },
    /// Look an address up in the published address index (newest first).
    AddrLookup {
        address: Address,
        /// Address of the SOC owner (the importer's key).
        #[arg(long, env = "ETH2SWARM_OWNER")]
        owner: Address,
        /// Chain ID used in the SOC identifiers.
        #[arg(long, env = "ETH2SWARM_CHAIN_ID", default_value_t = 1)]
        chain_id: u64,
        /// Read the head announcing this many units (default: the newest one found, aiming
        /// at the finalized block of --rpc).
        #[arg(long)]
        units: Option<u64>,
        /// Incremental sync: only what is new after this many units.
        #[arg(long, value_parser = clap::value_parser!(u64).range(0..=addr::layout::MAX_UNITS))]
        since: Option<u64>,
        /// Entries to print (all are counted).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Compare with the address's appearances detected directly from --rpc over the same
        /// blocks (with traces if the index has them); exits with an error on any difference,
        /// or if --rpc serves another chain.
        #[arg(long)]
        against_chain: bool,
    },
    /// Read the published address index back and check what one head makes a client read:
    /// every slice's buckets against its descriptor, every file against its reference, and
    /// every slice's last block against the chain (--rpc).
    AddrVerify {
        /// Address of the SOC owner (the importer's key).
        #[arg(long, env = "ETH2SWARM_OWNER")]
        owner: Address,
        /// Chain ID used in the SOC identifiers.
        #[arg(long, env = "ETH2SWARM_CHAIN_ID", default_value_t = 1)]
        chain_id: u64,
        /// The head announcing this many units (default: the origin's).
        #[arg(long)]
        units: Option<u64>,
    },
    /// Measure the address index with the real encoder: build every slice of levels
    /// 0..=LEVEL in the level-LEVEL window around each `--at` block and print the sizes per
    /// level as a Markdown table (nothing is uploaded).
    AddrStats {
        /// Window level (0 = one unit of 64 blocks, 8 = 16,384 blocks).
        #[arg(long, default_value_t = 5)]
        level: u8,
        /// A block in each window to measure (repeatable).
        #[arg(long = "at", required = true)]
        at: Vec<u64>,
        /// Also read `trace_block` (see `appearances`).
        #[arg(long)]
        traces: bool,
        /// Blocks fetched in parallel.
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
    },
    /// Build one address-index slice in memory and print its sizes (nothing is uploaded).
    AddrSlice {
        /// Slice level (0 = one unit of 64 blocks).
        level: u8,
        /// Slice index at that level.
        index: u32,
        /// Also read `trace_block` (see `appearances`).
        #[arg(long)]
        traces: bool,
    },
    /// List the address appearances in a block (docs/address-index.md, detection rules).
    Appearances {
        /// Block number.
        number: u64,
        /// Also read `trace_block` (Erigon, Reth, Anvil; not geth): internal calls,
        /// self-destructs, rewards, and words in internal calls' input and output.
        #[arg(long)]
        traces: bool,
    },
    /// RLP header sizes over a block range, grouped by fork shape (header field count).
    HeaderStats {
        #[arg(long, default_value_t = 0)]
        from: u64,
        /// Last block (inclusive). Defaults to the tip.
        #[arg(long)]
        to: Option<u64>,
        /// Sample every n-th block.
        #[arg(long, default_value_t = 1)]
        step: u64,
    },
    /// Import blocks 0..epochs*epoch_size into Swarm as SOCs plus an accumulator.
    Import {
        /// Private key of the SOC owner.
        #[arg(long, env = "ETH2SWARM_KEY", hide_env_values = true)]
        key: PrivateKeySigner,
        /// Blocks per epoch (power of two; Portal/era1 use 8192).
        #[arg(long, default_value_t = 64)]
        epoch_size: u32,
        /// Number of epochs to import, starting at block 0.
        #[arg(long, default_value_t = 4)]
        epochs: u32,
        /// Postage batch ID. Defaults to the Bee node's first usable batch.
        #[arg(long)]
        batch: Option<B256>,
    },
    /// Fetch a block from Swarm and verify header and body against the trust anchor.
    Get {
        number: u64,
        #[command(flatten)]
        source: Source,
    },
    /// Fetch an epoch's header file from Swarm and verify all its headers.
    VerifyEpoch {
        epoch: u64,
        #[command(flatten)]
        source: Source,
    },
    /// Local web UI to inspect SOCs, blocks, epochs and raw references on Swarm nodes.
    Serve {
        /// Address to listen on. The UI has no authentication: anything else than loopback
        /// lets whoever reaches the address read the configured nodes through it.
        /// `0.0.0.0:8090` listens on every address.
        #[arg(long, default_value = "127.0.0.1:8090")]
        listen: std::net::SocketAddr,
        /// Listen on this machine's Tailscale IPv4 address instead (the port from --listen),
        /// not on the LAN or loopback addresses, and accept its MagicDNS name as Host. On
        /// Linux a host on the same LAN can still route to that address unless a host firewall
        /// drops it: the bind alone does not make the UI tailnet-only.
        #[arg(long)]
        tailscale: bool,
        /// Further hosts the UI is opened by (repeatable), as `name`, `name:port`, `ip` or
        /// `ip:port`: a DNS name of this machine, a port forward (`localhost:9000` for
        /// `ssh -L 9000:…`), a proxy (`name:443`). The port defaults to the listen port. The
        /// address listened on needs no entry, nor does any IP address on the listen port when
        /// listening on every address; unknown names are refused (DNS rebinding).
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
        /// Nodes to read from, as `name=url` (repeatable). Defaults to antd on :1733 and the
        /// bee-factory nodes.
        #[arg(long = "node", value_parser = inspect::parse_node)]
        nodes: Vec<inspect::Node>,
        /// Default SOC owner shown in the UI.
        #[arg(long, env = "ETH2SWARM_OWNER")]
        owner: Option<Address>,
        /// Default chain ID shown in the UI.
        #[arg(long, env = "ETH2SWARM_CHAIN_ID", default_value_t = 1)]
        chain_id: u64,
        /// Default pinned trust anchor shown in the UI.
        #[arg(long, env = "ETH2SWARM_ANCHOR")]
        anchor: Option<B256>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr: stdout carries the commands' output (e.g. addr-stats' Markdown).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let provider = ProviderBuilder::new().connect_http(cli.rpc.clone());
    let bee = Bee::new(cli.bee.clone())?;

    match cli.command {
        Command::Tip => {
            let chain_id = provider.get_chain_id().await.context("querying chain id")?;
            let block = provider
                .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
                .await
                .context("querying latest block")?
                .context("chain returned no latest block")?;
            println!("RPC:        {}", cli.rpc);
            println!("Chain ID:   {chain_id}");
            println!("Tip:        #{}", block.header.number);
            println!("Block hash: {}", block.header.hash);
            println!("Timestamp:  {}", block.header.timestamp);
        }
        Command::Block { number } => {
            let block = provider
                .get_block_by_number(number.into())
                .await
                .with_context(|| format!("querying block {number}"))?
                .with_context(|| format!("block {number} not found"))?;
            println!("RPC:        {}", cli.rpc);
            println!("Number:     #{}", block.header.number);
            println!("Hash:       {}", block.header.hash);
            println!("Parent:     {}", block.header.parent_hash);
            println!("Timestamp:  {}", block.header.timestamp);
            println!("Txs:        {}", block.transactions.len());
            println!("Gas used:   {}", block.header.gas_used);
        }
        Command::AddrImport {
            key,
            state,
            units,
            follow,
            init,
            from_block,
            traces,
            batch,
            lower_batches,
            grace_days,
            out,
            defer_level,
            max_level,
            concurrency,
            sort_dir,
            sort_memory_mb,
        } => {
            let opts = addr::import::ImportOptions {
                state_dir: state,
                units,
                follow,
                init,
                from_block,
                traces,
                batch,
                lower_batches,
                grace_days,
                out,
                defer_level,
                max_level,
                concurrency,
                sort_dir,
                sort_buffer: usize::try_from(sort_memory_mb << 20)?,
            };
            let s = addr::import::run(&provider, &bee, &key, opts).await?;
            println!(
                "Address index of owner {} on chain {}: {} units, head update {}",
                s.owner, s.chain_id, s.units, s.head_index
            );
            print_batches(&s.batches, s.fell_back, "slices and heads", "lower slices");
        }
        Command::DevnetActivity {
            key,
            authority_key,
            busy,
            transfers,
            allow_chain_id,
        } => {
            let a = devnet::run(
                cli.rpc.clone(),
                key,
                authority_key,
                busy,
                transfers,
                allow_chain_id,
            )
            .await?;
            println!(
                "Activity in blocks {}..={}: {} transactions, emitter {}, busy {}, authority {}",
                a.first_block,
                a.last_block,
                a.transactions,
                a.emitter,
                a.busy,
                a.authority.map_or("none".to_string(), |x| x.to_string())
            );
        }
        Command::AddrUpload {
            dir,
            batch,
            lower_batches,
            grace_days,
            concurrency,
            from_start,
        } => {
            let batches = addr::export::Batches {
                permanent: batch,
                lower: lower_batches,
                grace_days: grace_days.unwrap_or(addr::stamps::GRACE_DAYS),
            };
            let s = addr::export::upload(&bee, batches, &dir, concurrency, from_start).await?;
            println!(
                "Uploaded the export of owner {} on chain {}: {} files, {} SOCs, {} heads \
                 ({} of {} bytes)",
                s.owner, s.chain_id, s.files, s.socs, s.heads, s.offset, s.committed
            );
            print_batches(
                &s.batches,
                s.fell_back,
                "records",
                "records of lower slices",
            );
        }
        Command::AddrLookup {
            address,
            owner,
            chain_id,
            units,
            since,
            limit,
            against_chain,
        } => {
            addr_lookup(
                &provider,
                &bee,
                (address, owner, chain_id),
                units,
                since,
                limit,
                against_chain,
            )
            .await?;
        }
        Command::AddrVerify {
            owner,
            chain_id,
            units,
        } => {
            let reader = addr::verify::IndexReader {
                bee: bee.clone(),
                owner,
                chain_id,
            };
            let r = addr::verify::verify_published(&reader, units).await?;
            addr::verify::check_chain(&provider, &r).await?;
            for ((level, index), root, _) in &r.slices {
                println!("  slice ({level}, {index}): buckets root {root}");
            }
            println!(
                "Verified head for {} units from block {} (pending {:?}): {} slices, {} \
                 buckets, {} files, {} addresses, {} appearances",
                r.units,
                r.first_unit * addr::layout::UNIT_BLOCKS,
                r.pending,
                r.slices.len(),
                r.buckets,
                r.files,
                r.addresses,
                r.appearances
            );
        }
        Command::AddrStats {
            level,
            at,
            traces,
            concurrency,
        } => {
            addr_stats(&provider, level, &at, traces, concurrency).await?;
        }
        Command::AddrSlice {
            level,
            index,
            traces,
        } => {
            addr_slice(&provider, level, index, traces).await?;
        }
        Command::Appearances { number, traces } => {
            let data = addr::fetch::fetch_block_data(&provider, number, traces).await?;
            let apps = addr::detect::detect(&data);
            for a in &apps {
                println!(
                    "{:>5}  {}  {}",
                    a.slot,
                    a.address,
                    addr::detect::role::names(a.roles)
                );
            }
            let addresses: std::collections::BTreeSet<_> = apps.iter().map(|a| a.address).collect();
            println!(
                "Block #{number}: {} txs, {} appearances, {} addresses{}",
                data.txs.len(),
                apps.len(),
                addresses.len(),
                if traces { " (with traces)" } else { "" }
            );
        }
        Command::HeaderStats { from, to, step } => {
            header_stats(&provider, from, to, step).await?;
        }
        Command::Import {
            key,
            epoch_size,
            epochs,
            batch,
        } => {
            let opts = import::ImportOptions {
                epoch_size,
                num_epochs: epochs,
                batch,
            };
            import::run(&provider, &bee, &key, opts).await?;
        }
        Command::Get { number, source } => {
            let (client, anchor) = client_for(&bee, &source).await?;
            let b = client.get_block(number, anchor).await?;
            println!("Verified #{} {}", b.header.number, b.hash);
            println!("  header RLP:   {} bytes", b.header_rlp_len);
            println!("  SOC payload:  {} bytes", b.soc_payload_len);
            println!("  txs:          {}", b.body.transactions.len());
            println!("  ommers:       {}", b.body.ommers.len());
            println!(
                "  withdrawals:  {}",
                b.body.withdrawals.as_ref().map_or(0, |w| w.len())
            );
        }
        Command::VerifyEpoch { epoch, source } => {
            let (client, anchor) = client_for(&bee, &source).await?;
            let headers = client.verify_epoch(epoch, anchor).await?;
            println!(
                "Verified epoch {epoch}: {} headers #{}..=#{}",
                headers.len(),
                headers.first().map_or(0, |h| h.number),
                headers.last().map_or(0, |h| h.number)
            );
        }
        Command::Serve {
            listen,
            tailscale,
            allow_hosts,
            nodes,
            owner,
            chain_id,
            anchor,
        } => {
            let nodes = if nodes.is_empty() {
                [
                    "antd=http://localhost:1733",
                    "queen=http://localhost:1633",
                    "worker1=http://localhost:1635",
                    "worker2=http://localhost:1637",
                    "worker3=http://localhost:1639",
                    "worker4=http://localhost:1641",
                ]
                .iter()
                .map(|s| inspect::parse_node(s).map_err(anyhow::Error::msg))
                .collect::<Result<_>>()?
            } else {
                nodes
            };
            let defaults = inspect::Defaults {
                owner,
                chain_id,
                anchor,
            };
            let mut listen = listen;
            let mut allow_hosts = allow_hosts;
            if tailscale {
                let (ip, name) = inspect::tailscale_identity()?;
                listen.set_ip(ip);
                allow_hosts.extend(name);
            }
            inspect::serve(listen, &allow_hosts, nodes, defaults).await?;
        }
    }
    Ok(())
}

async fn client_for<'a>(bee: &'a Bee, source: &Source) -> Result<(client::Client<'a>, B256)> {
    let client = client::Client {
        bee,
        owner: source.owner,
        chain_id: source.chain_id,
    };
    let anchor = match source.anchor {
        Some(a) => a,
        None => {
            let rec = client.published_anchor().await?;
            tracing::warn!(anchor = %rec.anchor, "no --anchor pinned; trusting the owner's published anchor");
            rec.anchor
        }
    };
    Ok((client, anchor))
}

async fn header_stats(
    provider: &impl Provider,
    from: u64,
    to: Option<u64>,
    step: u64,
) -> Result<()> {
    use std::collections::BTreeMap;
    ensure!(step > 0, "--step must be positive");
    let to = match to {
        Some(t) => t,
        None => provider.get_block_number().await?,
    };
    // field count → (count, min, max, sum, first block, last block)
    let mut groups: BTreeMap<usize, (u64, usize, usize, usize, u64, u64)> = BTreeMap::new();
    for n in (from..=to).step_by(step as usize) {
        let block = provider
            .get_block_by_number(n.into())
            .await?
            .with_context(|| format!("block {n} not found"))?;
        let header = block.header.inner;
        let len = alloy::rlp::encode(&header).len();
        let g =
            groups
                .entry(chain::header_field_count(&header))
                .or_insert((0, usize::MAX, 0, 0, n, n));
        g.0 += 1;
        g.1 = g.1.min(len);
        g.2 = g.2.max(len);
        g.3 += len;
        g.5 = n;
    }
    println!("fields  blocks          sampled   min   avg   max  per 4096-byte chunk");
    for (fields, (count, min, max, sum, first, last)) in groups {
        let avg = sum as f64 / count as f64;
        println!(
            "{fields:>6}  #{first}..#{last}  {count:>8} {min:>5} {avg:>5.0} {max:>5}  {}",
            4096 / max
        );
    }
    Ok(())
}

/// `addr-slice`: detect and build one slice, report its sizes.
async fn addr_slice<P>(provider: &P, level: u8, index: u32, traces: bool) -> Result<()>
where
    P: Provider + Clone + 'static,
{
    use addr::layout::{L_MAX, first_block, last_block};
    use swarm::file::file_chunks;

    ensure!(level <= L_MAX, "slice level {level} above {L_MAX}");
    let (first, last) = (first_block(level, index), last_block(level, index));
    let built = addr::fetch::fetch_slice(provider, (level, index), traces, 16).await?;
    let r = &built.record;
    let file_bytes: usize = built.files.iter().map(|(_, f)| f.len()).sum();
    let chunks: usize = built.files.iter().map(|(_, f)| file_chunks(f.len())).sum();
    let bucket_bytes: usize = built.buckets.iter().map(Vec::len).sum();
    println!("Slice ({level}, {index}): blocks {first}..={last}");
    println!("  appearances: {}", r.appearances);
    println!("  addresses:   {}", r.addresses);
    println!(
        "  buckets:     {} ({} paged), {} bytes, largest {} bytes",
        r.bucket_count,
        built.paged,
        bucket_bytes,
        built.buckets.iter().map(Vec::len).max().unwrap_or(0)
    );
    println!(
        "  files:       {} (spill and pages; {} spilled addresses), {} bytes, {} chunks",
        built.files.len(),
        built.spilled,
        file_bytes,
        chunks
    );
    println!(
        "  bytes per appearance: {:.2}",
        (bucket_bytes + file_bytes) as f64 / r.appearances.max(1) as f64
    );
    println!("  buckets root: {}", r.buckets_root);
    Ok(())
}

/// `addr-stats`: one Markdown table per window.
async fn addr_stats<P>(
    provider: &P,
    level: u8,
    at: &[u64],
    traces: bool,
    concurrency: usize,
) -> Result<()>
where
    P: Provider + Clone + 'static,
{
    use addr::detect::role;
    use addr::layout::{UNIT_BLOCKS, first_block, last_block};

    // A window is held in memory with all its appearances: a level-8 window at the tip is
    // ~25 M appearances (about 1 GB), and building a slice holds a map per address on top,
    // so a few GB at peak. Larger slices are built by merging, in the importer.
    const MAX_WINDOW_LEVEL: u8 = 8;
    ensure!(
        level <= MAX_WINDOW_LEVEL,
        "window level {level} above {MAX_WINDOW_LEVEL} (the window is held in memory)"
    );
    let roles = role::mask(traces);
    let tip = provider
        .get_block_number()
        .await
        .context("querying the chain tip")?;
    let mut windows: Vec<u32> = at
        .iter()
        .map(|b| u32::try_from((b / UNIT_BLOCKS) >> level))
        .collect::<Result<_, _>>()?;
    windows.sort_unstable();
    windows.dedup();
    // Every window must be complete before the first one is fetched.
    for &index in &windows {
        let last = last_block(level, index);
        ensure!(
            last <= tip,
            "window {}..={last} is not complete yet (the chain is at {tip})",
            first_block(level, index)
        );
    }
    for index in windows {
        let (first, last) = (first_block(level, index), last_block(level, index));
        eprintln!("fetching blocks {first}..={last}");
        let mut apps = Vec::new();
        let mut txs = 0u64;
        addr::fetch::fetch_appearances(provider, first..=last, traces, concurrency, |b| {
            apps.extend(b.appearances);
            txs += b.txs as u64;
            Ok(())
        })
        .await?;
        let w = addr::stats::window_stats((level, index), &apps, txs, roles)?;
        println!("{}", addr::stats::render(&w));
    }
    Ok(())
}

/// `addr-lookup`: newest entries of an address, optionally checked against the chain.
async fn addr_lookup<P>(
    provider: &P,
    bee: &Bee,
    (address, owner, chain_id): (Address, Address, u64),
    units: Option<u64>,
    since: Option<u64>,
    limit: usize,
    against_chain: bool,
) -> Result<()>
where
    P: Provider + Clone + 'static,
{
    use addr::detect::role;

    let reader = addr::verify::IndexReader {
        bee: bee.clone(),
        owner,
        chain_id,
    };
    let head = match units {
        Some(u) => reader.head_at(&reader.origin().await?, u).await?,
        None => {
            let finalized = provider
                .get_block_by_number(alloy::eips::BlockNumberOrTag::Finalized)
                .await?
                .context("the node reports no finalized block")?
                .header
                .number;
            let (head, miss) = addr::lookup::latest_head(&reader, finalized).await?;
            if let Some(reason) = miss {
                eprintln!(
                    "note: using the newest readable head ({} units), not the one for block \
                     {finalized}: {reason}. Usually the importer has not published it yet; \
                     Bee answers a missing and an unretrievable chunk alike.",
                    head.units
                );
            }
            head
        }
    };
    let found = addr::lookup::lookup(&reader, address, &head, since).await?;
    for e in found.entries.iter().take(limit) {
        println!("{:>10}  {:>5}  {}", e.block, e.slot, role::names(e.roles));
    }
    println!(
        "{address}: {} appearances in the index in blocks {}..={} ({} units{}; {} slices, {} \
         files read)",
        found.entries.len(),
        found.first_block,
        head.last_block(),
        head.units,
        since.map_or(String::new(), |m| format!(", new since unit {m}")),
        found.slices,
        found.files
    );
    if head.first_unit > 0 {
        println!(
            "The index starts at block {}: blocks before it are not covered.",
            head.first_block()
        );
    }
    if against_chain {
        let c = addr::lookup::against_chain(provider, address, &found, &head, chain_id).await?;
        ensure!(
            c.not_on_chain.is_empty() && c.missing.is_empty(),
            "index and chain differ for {address}: {} entries not on the chain {:?}, {} \
             appearances missing {:?}",
            c.not_on_chain.len(),
            c.not_on_chain.iter().take(5).collect::<Vec<_>>(),
            c.missing.len(),
            c.missing.iter().take(5).collect::<Vec<_>>()
        );
        println!("Matches the chain: every entry is on it, none is missing.");
    }
    Ok(())
}

/// The batches a run stamped with, and how many lower-slice items fell back to the permanent
/// batch.
fn print_batches(
    batches: &std::collections::BTreeMap<B256, u64>,
    fell_back: u64,
    what: &str,
    lower: &str,
) {
    for (b, n) in batches {
        println!("  batch {b}: {n} {what}");
    }
    if fell_back > 0 {
        println!(
            "  {fell_back} {lower} went to the permanent batch: no --lower-batch lived long enough"
        );
    }
}
