mod accumulator;
mod addr;
mod chain;
mod checkpoints;
mod client;
mod devnet;
mod import;
mod inspect;
mod layout;
mod node_view;
mod stamps;
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

    /// A node for the receipts of blocks before Byzantium (mainnet block 4,370,000), such as
    /// geth: those receipts hold the state root after their transaction, which Erigon 3 does
    /// not serve, so their root cannot be checked against the header from its answer.
    /// Everything else still comes from --rpc, and these receipts are checked like any other.
    #[arg(long, global = true, env = "ETH2SWARM_RECEIPTS_RPC")]
    receipts_rpc: Option<Url>,

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
/// How uploads are stamped (`addr-import`, `addr-upload`; see `docs/address-index.md`,
/// "Lifetime").
#[derive(clap::Args, Debug, Clone)]
struct Stamping {
    /// Postage batch ID for the permanent records: heads, the origin, top-level slices, and
    /// everything without --lower-batch. Required unless the Bee node is bee-factory (its
    /// GET /wallet reports chain 1337), where it defaults to the batch of the earlier uploads
    /// of the directory (addr-upload), else the node's first usable immutable batch; required
    /// with --lower-batch everywhere.
    #[arg(long)]
    batch: Option<B256>,
    /// A batch for the lower slices, which die once the head has passed them (repeatable).
    /// Each lower slice goes to the one that expires soonest after the slice is dead plus
    /// --grace-days, or to --batch if none lives that long. Batches are never bought or
    /// topped up.
    #[arg(long = "lower-batch")]
    lower_batches: Vec<B256>,
    /// Days a lower batch must outlive a slice (default 7): for clients holding an older
    /// head, a deferred merge still being built (whose expansion stays in use meanwhile), the
    /// importer being stopped, and for an export the time between two uploads.
    #[arg(long)]
    grace_days: Option<u64>,
    /// Seconds per block of the chain (default 12, mainnet's slot time): how long a unit, and
    /// so a lower slice's lifetime, lasts.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    slot_secs: Option<u64>,
}

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
enum StampsCommand {
    /// The postage batch an export needs: every chunk its upload stamps, counted per bucket
    /// (a chunk address's first 16 bits), the smallest depth that holds the fullest bucket
    /// with room to spare, and with `--days` the cost at the node's current price and the
    /// command to buy it. With lower slices in the export, also the permanent batch's and a
    /// lower batch's share.
    Plan {
        /// The export directory.
        dir: std::path::PathBuf,
        /// Price a lifetime of this many days (reads the price from the Bee node).
        #[arg(long)]
        days: Option<u64>,
        /// The highest share of its capacity the fullest bucket may take (0 to 1): room for
        /// what an index adds later, and a plan that is not exact to the chunk.
        #[arg(long, default_value_t = 0.5)]
        max_fill: f64,
        /// Seconds per block of the chain the postage contract is on (Gnosis Chain: 5).
        #[arg(long, default_value_t = stamps::DEFAULT_BLOCK_SECS,
              value_parser = clap::value_parser!(u64).range(1..))]
        block_secs: u64,
        /// The label for the buy command (owner, chain and first block of the index).
        #[arg(long)]
        label: Option<String>,
    },
}

#[derive(Subcommand)]
enum Command {
    /// The hash of every 1024th block from genesis up to a finalized block (Myotis's
    /// checkpoint table, its docs/ideas/deep-block-verification.md): every header is hashed
    /// here and linked by parentHash down to the genesis hash; the node's hashes are never
    /// used. Writes <out> (32 bytes per entry, entry i = block i·1024) and <out>.json. A full
    /// build resumes from <out>.work after a stop; --extend continues an existing table from
    /// its last entry up to a new top.
    CheckpointTable {
        /// The table file to write.
        #[arg(long)]
        out: std::path::PathBuf,
        /// Extend this existing table (with its <table>.json) instead of walking from genesis:
        /// the walk starts at its last entry, which it must reproduce, passes through its
        /// recorded top, and goes up to the new top; its entries are kept and the new ones
        /// appended. --out must be another file; the old pair is never changed. For weeks of
        /// blocks (it keeps them in memory and does not resume): a full build resumes.
        #[arg(long, conflicts_with = "genesis_hash")]
        extend: Option<std::path::PathBuf>,
        /// The table's new top: the block a full build walks down from, or an extension walks
        /// up to (default: the node's finalized block).
        #[arg(long, requires = "top_hash")]
        top: Option<u64>,
        /// The top block's hash, from a source you trust (a beacon-attested finalized block).
        #[arg(long, requires = "top")]
        top_hash: Option<B256>,
        /// The network's genesis hash (default: mainnet's, on chain 1).
        #[arg(long)]
        genesis_hash: Option<B256>,
        /// Chunks of 1024 headers fetched in parallel.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Before anything is written, compare about 200 entries, the last one and the top
        /// block with this node (another client than --rpc). Pins the top in place of
        /// --top-hash; a run with neither is refused, and a mismatch writes nothing.
        #[arg(long)]
        cross_check: Option<Url>,
    },
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
        /// Read TRACE from an archive node's frame index instead: a directory of A1TR files
        /// written by tools/erigon-trace-export, exact under the TRACE rule (every frame at
        /// entry). Each file must be for this chain, and each block's hash and transaction
        /// count must match the block fetched from --rpc. The files must cover every block
        /// from the index's start to --units without a gap (checked before anything is
        /// fetched), and stay: merges read imported blocks again. Not with --follow.
        #[arg(long, conflicts_with_all = ["traces", "follow"])]
        trace_index: Option<std::path::PathBuf>,
        #[command(flatten)]
        stamping: Stamping,
        /// Write everything to this directory instead of uploading it (no Bee node needed);
        /// `addr-upload` pushes it through a Bee node later. Every run of the index then
        /// passes the same directory. Keep it apart from --state.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Merges of this level and above are built in the background.
        #[arg(long, default_value_t = 6)]
        defer_level: u8,
        /// Highest slice level built (default: every level, up to the top level 16). Memory is
        /// bounded by --sort-memory-mb at any level (a busy address's spill file is written
        /// page by page, and above 1 MiB uploaded chunk by chunk; an export started before
        /// layout 3 writes it whole and refuses one above 256 MB); a lower cap bounds the time
        /// and the temporary disk one slice takes, and refuses the run once a slice would hold
        /// more: a backfill from genesis at once, an index that follows the chain when its
        /// merges reach the cap.
        #[arg(long, default_value_t = addr::layout::L_MAX)]
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
        #[command(flatten)]
        stamping: Stamping,
        /// Records uploaded in parallel (between two heads).
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
        /// Upload everything again, ignoring the progress of earlier uploads.
        #[arg(long)]
        from_start: bool,
        /// Skip the check that the batches can take the upload. The check counts a chunk the
        /// export lists twice, or one a stopped upload already sent, twice: for a batch within
        /// a few chunks of full it can refuse what Bee would take.
        #[arg(long)]
        no_capacity_check: bool,
    },
    /// The Bee node's postage batches: depth, the fill of the fullest bucket against its
    /// capacity, the immutable flag and the remaining lifetime (docs/address-index.md,
    /// "Lifetime").
    /// With `--export` and `--state`, which batches each of those indexes recorded. `stamps plan`
    /// sizes and prices a new batch for an export. Nothing is bought, topped up or diluted:
    /// that is the owner's action.
    #[command(args_conflicts_with_subcommands = true)]
    Stamps {
        #[command(subcommand)]
        plan: Option<StampsCommand>,
        /// An export directory (`addr-import --out`) to name the batches of: the ones its
        /// uploads recorded (repeatable).
        #[arg(long = "export")]
        exports: Vec<std::path::PathBuf>,
        /// An importer state directory (`addr-import --state`) of an index uploaded straight
        /// to Bee, to name the batches of (repeatable).
        #[arg(long = "state")]
        states: Vec<std::path::PathBuf>,
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
        /// With --against-chain on an index with TRACE: take TRACE from this frame index (A1TR
        /// files) instead of `trace_block`, which misses the target of a failed internal
        /// creation.
        #[arg(long, requires = "against_chain")]
        trace_index: Option<std::path::PathBuf>,
        /// With --against-chain: blocks fetched from --rpc in parallel (nuc's archive node:
        /// at most 4 beside other load).
        #[arg(long, default_value_t = 16, requires = "against_chain")]
        concurrency: usize,
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
    /// Compare an archive node's frame index (A1TR files from tools/erigon-trace-export) with
    /// `trace_block` over a block range, one block at a time: the TRACE appearances each gives.
    /// An appearance only `trace_block` has is an error; one only the index has is listed as
    /// a candidate failed internal creation, whose target `trace_block` does not name (the
    /// TRACE rule counts it). Every block's hash and transaction count are checked too.
    TraceCompare {
        /// The directory of A1TR files.
        #[arg(long)]
        trace_index: std::path::PathBuf,
        /// First block.
        #[arg(long)]
        from: u64,
        /// Last block.
        #[arg(long)]
        to: u64,
    },
    /// List the address appearances in a block (docs/address-index.md, detection rules).
    Appearances {
        /// Block number.
        number: u64,
        /// Also read `trace_block` (Erigon, Reth, Anvil; not geth): the addresses of every call
        /// frame, creations, self-destructs and rewards (no words from trace data).
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
        /// Postage batch ID (immutable). Required unless the Bee node is bee-factory (its
        /// GET /wallet reports chain 1337), where it defaults to the node's first usable
        /// immutable batch.
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
    /// Local web UI to inspect SOCs, blocks, epochs, raw references and the address index on
    /// Swarm nodes, and a node's wallet, chequebook and postage batches (read only).
    Serve {
        /// Address to listen on. The UI has no authentication: anything else than loopback
        /// lets whoever reaches the address read the configured nodes through it, their
        /// balances included.
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
    if let Some(url) = &cli.receipts_rpc {
        addr::fetch::set_receipts_rpc(url.clone())?;
    }
    let bee = Bee::new(cli.bee.clone())?;

    match cli.command {
        Command::CheckpointTable {
            out,
            extend: Some(from),
            top,
            top_hash,
            concurrency,
            cross_check,
            ..
        } => {
            let meta = checkpoints::extend(&checkpoints::ExtendOptions {
                rpc: cli.rpc.to_string(),
                from,
                out,
                top,
                top_hash,
                concurrency,
                cross_check: cross_check.as_ref().map(|u| u.to_string()),
            })
            .await?;
            println!("{}", serde_json::to_string_pretty(&meta)?);
        }
        Command::CheckpointTable {
            out,
            extend: None,
            top,
            top_hash,
            genesis_hash,
            concurrency,
            cross_check,
        } => {
            let meta = checkpoints::build(&checkpoints::Options {
                rpc: cli.rpc.to_string(),
                out: out.clone(),
                top,
                top_hash,
                genesis: genesis_hash,
                concurrency,
                cross_check: cross_check.as_ref().map(|u| u.to_string()),
            })
            .await?;
            println!("{}", serde_json::to_string_pretty(&meta)?);
        }
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
            trace_index,
            stamping,
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
                trace_index,
                batch: stamping.batch,
                lower_batches: stamping.lower_batches,
                grace_days: stamping.grace_days,
                slot_secs: stamping.slot_secs,
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
                "Activity in blocks {}..={}: {} transactions, emitter {}, busy {}, authority {}, \
                 failed creation {}",
                a.first_block,
                a.last_block,
                a.transactions,
                a.emitter,
                a.busy,
                a.authority.map_or("none".to_string(), |x| x.to_string()),
                a.failed_creation
            );
        }
        Command::Stamps {
            plan: None,
            exports,
            states,
        } => stamps::list(&bee, &exports, &states).await?,
        Command::Stamps {
            plan:
                Some(StampsCommand::Plan {
                    dir,
                    days,
                    max_fill,
                    block_secs,
                    label,
                }),
            ..
        } => {
            stamps::plan(
                &bee,
                &dir,
                &stamps::PlanOptions {
                    days,
                    max_fill,
                    block_secs,
                    label,
                },
            )
            .await?
        }
        Command::AddrUpload {
            dir,
            stamping,
            concurrency,
            from_start,
            no_capacity_check,
        } => {
            let batches = addr::export::Batches {
                permanent: stamping.batch,
                lower: stamping.lower_batches,
                grace_days: stamping.grace_days.unwrap_or(addr::stamps::GRACE_DAYS),
                slot_secs: stamping.slot_secs.unwrap_or(addr::stamps::SLOT_SECS),
                skip_capacity_check: no_capacity_check,
            };
            let s = addr::export::upload(&bee, batches, &dir, concurrency, from_start).await?;
            println!(
                "Uploaded the export of owner {} on chain {}: {} files, {} chunks of large \
                 files, {} SOCs, {} heads ({} of {} bytes)",
                s.owner, s.chain_id, s.files, s.chunks, s.socs, s.heads, s.offset, s.committed
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
            trace_index,
            concurrency,
        } => {
            addr_lookup(
                &provider,
                &bee,
                (address, owner, chain_id),
                units,
                since,
                limit,
                against_chain.then_some((trace_index.as_deref(), concurrency)),
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
        Command::TraceCompare {
            trace_index,
            from,
            to,
        } => {
            let s = trace_compare(&provider, &trace_index, from, to).await?;
            println!(
                "Blocks {from}..={to}: {} TRACE appearances from trace_block, {} from the index; \
                 {} only in the index (candidate failed internal creations)",
                s.rpc, s.index, s.index_only
            );
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
    let built = addr::fetch::fetch_slice(
        provider,
        (level, index),
        &addr::fetch::Traces::rpc(traces),
        16,
    )
    .await?;
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
    // ~31 M appearances (about 1.2 GB; docs/address-index.md, *Measurements*), and building
    // a slice holds a map per address on top, so a few GB at peak. Larger slices are built by merging, in the importer.
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
    // Progress on stderr every 30 s: the window's and the whole run's (stdout is the tables).
    let per_window = last_block(level, 0) - first_block(level, 0) + 1;
    let count = windows.len();
    let now = std::time::Instant::now;
    let mut run = addr::stats::Progress::new(per_window * count as u64, now());
    for (k, index) in windows.into_iter().enumerate() {
        let (first, last) = (first_block(level, index), last_block(level, index));
        eprintln!("window {}/{count}: fetching blocks {first}..={last}", k + 1);
        let mut window = addr::stats::Progress::new(per_window, now());
        let mut apps = Vec::new();
        let mut txs = 0u64;
        addr::fetch::fetch_appearances(
            provider,
            first..=last,
            &addr::fetch::Traces::rpc(traces),
            concurrency,
            |b| {
                apps.extend(b.appearances);
                txs += b.txs as u64;
                let t = now();
                run.count();
                if let Some(line) = window.block(t) {
                    eprintln!("  window {}/{count}: {line}", k + 1);
                    // The run's line goes with the window's, on the same 30 s.
                    eprintln!("  whole run: {}", run.line(t));
                }
                Ok(())
            },
        )
        .await?;
        eprintln!(
            "window {}/{count} done; whole run: {}",
            k + 1,
            run.line(now())
        );
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
    // Compare with the chain (with this frame index for TRACE if given, and this many blocks
    // fetched at a time).
    against_chain: Option<(Option<&std::path::Path>, usize)>,
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
    if let Some((trace_index, concurrency)) = against_chain {
        let c = addr::lookup::against_chain(
            provider,
            address,
            &found,
            &head,
            chain_id,
            trace_index,
            concurrency,
        )
        .await?;
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

/// What [`trace_compare`] counted.
struct TraceComparison {
    rpc: u64,
    index: u64,
    index_only: u64,
}

/// The TRACE appearances of every block in `from..=to` from `trace_block` and from the frame
/// index in `dir`, compared (see `Command::TraceCompare`).
async fn trace_compare<P: Provider>(
    provider: &P,
    dir: &std::path::Path,
    from: u64,
    to: u64,
) -> Result<TraceComparison> {
    use addr::detect::{detect, detect_with_frames, role};
    ensure!(from <= to, "--from {from} is after --to {to}");
    let chain_id = provider.get_chain_id().await.context("querying chain id")?;
    let index = addr::trace_index::TraceIndex::open(dir, chain_id)?;
    for n in [from, to] {
        ensure!(
            index.ranges().iter().any(|r| r.contains(&n)),
            "the index does not cover block {n} (it covers {:?})",
            index.ranges()
        );
    }
    let trace =
        |apps: Vec<addr::detect::Appearance>| -> std::collections::BTreeSet<(u32, Address)> {
            apps.into_iter()
                .filter(|a| a.roles & role::TRACE != 0)
                .map(|a| (a.slot, a.address))
                .collect()
        };
    let mut s = TraceComparison {
        rpc: 0,
        index: 0,
        index_only: 0,
    };
    let mut missing = Vec::new();
    for n in from..=to {
        let data = addr::fetch::fetch_block_data(provider, n, true).await?;
        let entry = index.block(n)?;
        ensure!(
            entry.hash == data.hash && entry.tx_count as usize == data.txs.len(),
            "block {n}: the index has hash {} and {} transactions, the node {} and {}",
            entry.hash,
            entry.tx_count,
            data.hash,
            data.txs.len()
        );
        let rpc = trace(detect(&data));
        let plain = addr::detect::BlockData {
            traces: None,
            ..data
        };
        let idx = trace(detect_with_frames(&plain, Some(&entry.frames)));
        for (slot, a) in rpc.difference(&idx) {
            missing.push(format!("block {n} tx {} {a}", slot - 1));
        }
        for (slot, a) in idx.difference(&rpc) {
            println!("only in the index: block {n} tx {} {a}", slot - 1);
            s.index_only += 1;
        }
        s.rpc += rpc.len() as u64;
        s.index += idx.len() as u64;
    }
    ensure!(
        missing.is_empty(),
        "{} TRACE appearances from trace_block are missing in the index: {}",
        missing.len(),
        missing.join("; ")
    );
    Ok(s)
}
