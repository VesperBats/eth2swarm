mod accumulator;
mod addr;
mod chain;
mod client;
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
        /// Address to listen on (keep it on loopback: the UI has no authentication).
        #[arg(long, default_value = "127.0.0.1:8090")]
        listen: std::net::SocketAddr,
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
    tracing_subscriber::fmt()
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
            inspect::serve(listen, nodes, defaults).await?;
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
    use addr::detect::{detect, role};
    use addr::layout::{BucketBody, BucketRecord, L_MAX, PAGE_SIZE, first_block, last_block};

    ensure!(level <= L_MAX, "slice level {level} above {L_MAX}");
    let (first, last) = (first_block(level, index), last_block(level, index));
    let blocks = addr::fetch::fetch_range(provider, first..=last, traces, 16).await?;
    let last_hash = blocks.last().map(|b| b.hash).unwrap_or_default();
    let apps: Vec<_> = blocks.iter().flat_map(detect).collect();
    let roles = if traces { 0xff } else { !role::TRACE };
    let built = addr::slice::build_slice((level, index), &apps, last_hash, roles)?;
    let r = &built.record;
    let paged = built
        .buckets
        .iter()
        .filter(|b| {
            matches!(
                BucketRecord::decode(b).map(|b| b.body),
                Ok(BucketBody::Paged { .. })
            )
        })
        .count();
    let file_bytes: usize = built.files.iter().map(Vec::len).sum();
    let file_chunks: usize = built
        .files
        .iter()
        .map(|f| f.len().div_ceil(PAGE_SIZE))
        .sum();
    let bucket_bytes: usize = built.buckets.iter().map(Vec::len).sum();
    println!("Slice ({level}, {index}): blocks {first}..={last}");
    println!("  appearances: {}", r.appearances);
    println!("  addresses:   {}", r.addresses);
    println!(
        "  buckets:     {} ({} paged), {} bytes, largest {} bytes",
        r.bucket_count,
        paged,
        bucket_bytes,
        built.buckets.iter().map(Vec::len).max().unwrap_or(0)
    );
    println!(
        "  files:       {} (spill and pages), {} bytes, {} data chunks",
        built.files.len(),
        file_bytes,
        file_chunks
    );
    println!(
        "  bytes per appearance: {:.2}",
        (bucket_bytes + file_bytes) as f64 / r.appearances.max(1) as f64
    );
    println!("  buckets root: {}", r.buckets_root);
    Ok(())
}
