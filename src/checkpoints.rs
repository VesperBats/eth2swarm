//! `ethswarm checkpoint-table`: the hash of every 1024th block, from genesis up to a finalized
//! block, for Myotis's deep-block verification (its `docs/ideas/deep-block-verification.md`,
//! decision D1: a bundled table, owner's decision 2026-11-20).
//!
//! The node is asked for headers only, and nothing it says about a hash is used: every header
//! is RLP-encoded and hashed here, each one's `parentHash` must be the hash computed for the
//! block below it, the walk must end at the network's genesis hash, and the top block's hash
//! must be the one the caller pins (`--top-hash`, for example a beacon-attested finalized
//! block), or else the node's finalized block, which is then recorded as such. So the table is
//! as good as the chain of hashes, not as the node.
//!
//! The work is done in chunks of 1024 blocks, fetched in parallel; each finished chunk's
//! summary (its first block's parent hash, its first and last block's hashes) is appended to a
//! work file, so a run that stops resumes where it was. Once every chunk is in, the chunks are
//! linked to each other, and the table is written: entry `i` is the hash of block `i·1024`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use alloy::primitives::{B256, keccak256};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

/// Blocks per table entry: geth's `GetBlockHeaders` limit, so one request reaches any block.
pub const K: u64 = 1024;
/// Headers per JSON-RPC batch: Erigon's default `--rpc.batch.limit`.
const BATCH: u64 = 100;
/// Mainnet's genesis hash.
pub const MAINNET_GENESIS: B256 =
    alloy::primitives::b256!("d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3");

/// What a finished chunk of blocks `c·K ..= min(c·K + K − 1, top)` says, from headers hashed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chunk {
    /// The `parentHash` of the chunk's first block.
    parent: B256,
    /// The hash of the chunk's first block (a table entry).
    first: B256,
    /// The hash of the chunk's last block.
    last: B256,
}

const WORK_RECORD: usize = 8 + 32 * 3;

impl Chunk {
    fn encode(&self, index: u64) -> [u8; WORK_RECORD] {
        let mut r = [0u8; WORK_RECORD];
        r[..8].copy_from_slice(&index.to_be_bytes());
        r[8..40].copy_from_slice(self.parent.as_slice());
        r[40..72].copy_from_slice(self.first.as_slice());
        r[72..].copy_from_slice(self.last.as_slice());
        r
    }

    fn decode(r: &[u8]) -> (u64, Self) {
        (
            u64::from_be_bytes(r[..8].try_into().unwrap()),
            Self {
                parent: B256::from_slice(&r[8..40]),
                first: B256::from_slice(&r[40..72]),
                last: B256::from_slice(&r[72..]),
            },
        )
    }
}

/// Fetches headers `from ..= to` in one JSON-RPC batch and returns their hashes, computed here,
/// checking every `parentHash` link inside the range. Returns (first block's parent, hashes).
async fn fetch_range(
    http: &reqwest::Client,
    rpc: &str,
    from: u64,
    to: u64,
) -> Result<(B256, Vec<B256>)> {
    let body: Vec<serde_json::Value> = (from..=to)
        .map(|n| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": n,
                "method": "eth_getHeaderByNumber",
                "params": [format!("{n:#x}")],
            })
        })
        .collect();
    let answers: Vec<serde_json::Value> = http
        .post(rpc)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("asking for headers {from}..={to}"))?
        .error_for_status()?
        .json()
        .await
        .with_context(|| format!("reading headers {from}..={to}"))?;
    let mut by_number: BTreeMap<u64, alloy::rpc::types::Header> = BTreeMap::new();
    for a in answers {
        let id = a["id"]
            .as_u64()
            .with_context(|| format!("an answer without a numeric id: {a}"))?;
        if let Some(e) = a.get("error") {
            bail!("header {id}: {e}");
        }
        let header: alloy::rpc::types::Header = serde_json::from_value(a["result"].clone())
            .with_context(|| format!("header {id}: {}", a["result"]))?;
        by_number.insert(id, header);
    }
    ensure!(
        by_number.len() as u64 == to - from + 1,
        "headers {from}..={to}: {} answers",
        by_number.len()
    );
    let mut hashes = Vec::with_capacity(by_number.len());
    let mut parent = B256::ZERO;
    for (n, header) in &by_number {
        ensure!(
            header.inner.number == *n,
            "asked for header {n}, got {}",
            header.inner.number
        );
        // The hash is computed, never taken from the node.
        let hash = keccak256(alloy::rlp::encode(&header.inner));
        ensure!(
            hash == header.hash,
            "header {n}: hashes to {hash} here, the node says {} (an encoding this build \
             does not know?)",
            header.hash
        );
        match hashes.last() {
            None => parent = header.inner.parent_hash,
            Some(prev) => ensure!(
                header.inner.parent_hash == *prev,
                "header {n}: parentHash {} is not header {}'s hash {prev}",
                header.inner.parent_hash,
                n - 1
            ),
        }
        hashes.push(hash);
    }
    Ok((parent, hashes))
}

/// One chunk: its blocks in batches, linked to each other.
async fn fetch_chunk(http: &reqwest::Client, rpc: &str, index: u64, top: u64) -> Result<Chunk> {
    let (from, to) = (index * K, (index * K + K - 1).min(top));
    let mut parent = None;
    let mut all: Vec<B256> = Vec::with_capacity((to - from + 1) as usize);
    let mut at = from;
    while at <= to {
        let end = (at + BATCH - 1).min(to);
        let mut attempt = 0;
        let (p, hashes) = loop {
            match fetch_range(http, rpc, at, end).await {
                Ok(r) => break r,
                Err(e) if attempt < 3 => {
                    attempt += 1;
                    tracing::warn!("headers {at}..={end}, attempt {attempt}: {e:#}");
                    tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
                }
                Err(e) => return Err(e),
            }
        };
        match all.last() {
            None => parent = Some(p),
            Some(prev) => ensure!(
                p == *prev,
                "header {at}: parentHash {p} is not header {}'s hash {prev}",
                at - 1
            ),
        }
        all.extend(hashes);
        at = end + 1;
    }
    Ok(Chunk {
        parent: parent.expect("a chunk has a block"),
        first: all[0],
        last: *all.last().unwrap(),
    })
}

/// What the table's sidecar records.
#[derive(Debug, Serialize)]
pub struct TableMeta {
    pub chain_id: u64,
    /// Blocks per entry.
    pub k: u64,
    /// Entry `i` is the hash of block `i·k`, for `i` in `0..entries`.
    pub entries: u64,
    pub last_entry_block: u64,
    /// The block the walk started from, its hash, and what pinned it.
    pub top_block: u64,
    pub top_hash: B256,
    pub top_pinned_by: String,
    pub genesis_hash: B256,
    /// keccak256 of the table file.
    pub table_keccak256: B256,
    /// `web3_clientVersion` of the node the headers came from (they are checked, not trusted).
    pub source_client: String,
    pub generated_unix: u64,
}

/// Options of a run.
pub struct Options {
    pub rpc: String,
    pub out: PathBuf,
    /// The block to walk down from (default: the node's finalized block).
    pub top: Option<u64>,
    /// Its hash, from a source the caller trusts (for example a beacon-attested finalized
    /// block). Required with `top`.
    pub top_hash: Option<B256>,
    /// The network's genesis hash (default: mainnet's, on chain 1).
    pub genesis: Option<B256>,
    pub concurrency: usize,
}

async fn rpc_call(
    http: &reqwest::Client,
    rpc: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let v: serde_json::Value = http
        .post(rpc)
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await
        .with_context(|| method.to_string())?
        .error_for_status()?
        .json()
        .await?;
    if let Some(e) = v.get("error") {
        bail!("{method}: {e}");
    }
    Ok(v["result"].clone())
}

fn work_path(out: &Path) -> PathBuf {
    let mut p = out.as_os_str().to_owned();
    p.push(".work");
    PathBuf::from(p)
}

/// Builds the table: walks every header from the top block down to genesis.
pub async fn build(opts: &Options) -> Result<TableMeta> {
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let rpc = opts.rpc.as_str();
    let chain_id = u64::from_str_radix(
        rpc_call(&http, rpc, "eth_chainId", serde_json::json!([]))
            .await?
            .as_str()
            .context("eth_chainId")?
            .trim_start_matches("0x"),
        16,
    )?;
    let genesis = match (opts.genesis, chain_id) {
        (Some(g), _) => g,
        (None, 1) => MAINNET_GENESIS,
        (None, c) => bail!("chain {c}: pass its genesis hash with --genesis-hash"),
    };
    let client = rpc_call(&http, rpc, "web3_clientVersion", serde_json::json!([]))
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into());
    let (top, top_hash, pinned) = match (opts.top, opts.top_hash) {
        (Some(t), Some(h)) => (t, h, "the caller (--top-hash)".to_string()),
        (Some(_), None) | (None, Some(_)) => bail!("--top and --top-hash go together"),
        (None, None) => {
            let f = rpc_call(
                &http,
                rpc,
                "eth_getHeaderByNumber",
                serde_json::json!(["finalized"]),
            )
            .await?;
            let header: alloy::rpc::types::Header = serde_json::from_value(f)?;
            let hash = keccak256(alloy::rlp::encode(&header.inner));
            (
                header.inner.number,
                hash,
                format!("the node's finalized block ({client})"),
            )
        }
    };
    let chunks = top / K + 1;

    // Resume: the chunks a previous run finished, for this same top.
    let work = work_path(&opts.out);
    let mut done: BTreeMap<u64, Chunk> = BTreeMap::new();
    let tag = format!("top {top} {top_hash}\n");
    match std::fs::read(&work) {
        Ok(bytes) if bytes.starts_with(tag.as_bytes()) => {
            // A record cut short by a stop is left out (that chunk is fetched again).
            for r in bytes[tag.len()..].as_chunks::<WORK_RECORD>().0 {
                let (i, c) = Chunk::decode(r);
                done.insert(i, c);
            }
            tracing::info!(done = done.len(), chunks, "resuming");
        }
        Ok(_) => bail!(
            "{} is the work of another run (another top block): remove it to start over",
            work.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&work, tag.as_bytes())?;
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", work.display())),
    }
    let mut work_file = std::fs::OpenOptions::new().append(true).open(&work)?;

    // Newest first: the header encodings of the latest forks are checked within minutes, not
    // at the end of the run.
    let todo: Vec<u64> = (0..chunks).rev().filter(|i| !done.contains_key(i)).collect();
    let started = Instant::now();
    let mut last_report = Instant::now();
    let (total, mut finished) = (todo.len() as u64, 0u64);
    let mut queue = todo.into_iter();
    let mut set = tokio::task::JoinSet::new();
    let concurrency = opts.concurrency.max(1);
    loop {
        while set.len() < concurrency {
            let Some(i) = queue.next() else { break };
            let (http, rpc) = (http.clone(), opts.rpc.clone());
            set.spawn(async move { (i, fetch_chunk(&http, &rpc, i, top).await) });
        }
        let Some(joined) = set.join_next().await else {
            break;
        };
        let (i, chunk) = joined.context("a chunk task panicked")?;
        let chunk = chunk.with_context(|| format!("chunk {i} (blocks from {})", i * K))?;
        work_file.write_all(&chunk.encode(i))?;
        done.insert(i, chunk);
        finished += 1;
        if last_report.elapsed().as_secs() >= 30 || finished == total {
            last_report = Instant::now();
            work_file.flush()?;
            let secs = started.elapsed().as_secs_f64().max(1e-9);
            let rate = finished as f64 / secs;
            let left = (total - finished) as f64 / rate.max(1e-9);
            eprintln!(
                "{finished}/{total} chunks ({} blocks/s), about {}h{:02}m left",
                (rate * K as f64) as u64,
                left as u64 / 3600,
                left as u64 % 3600 / 60
            );
        }
    }
    work_file.sync_all()?;

    // Link the chunks: every chunk's first parent is the last hash of the chunk below.
    ensure!(
        done.len() as u64 == chunks,
        "{} of {chunks} chunks",
        done.len()
    );
    let first = &done[&0];
    ensure!(
        first.first == genesis,
        "block 0 hashes to {}, the genesis hash is {genesis}",
        first.first
    );
    for i in 1..chunks {
        let (below, this) = (&done[&(i - 1)], &done[&i]);
        ensure!(
            this.parent == below.last,
            "block {}: parentHash {} is not block {}'s hash {}",
            i * K,
            this.parent,
            i * K - 1,
            below.last
        );
    }
    let last = &done[&(chunks - 1)];
    ensure!(
        last.last == top_hash,
        "block {top} hashes to {}, the top is pinned to {top_hash}",
        last.last
    );

    let mut table = Vec::with_capacity(chunks as usize * 32);
    for i in 0..chunks {
        table.extend_from_slice(done[&i].first.as_slice());
    }
    let tmp = opts.out.with_extension("tmp");
    std::fs::write(&tmp, &table)?;
    std::fs::rename(&tmp, &opts.out)?;
    let meta = TableMeta {
        chain_id,
        k: K,
        entries: chunks,
        last_entry_block: (chunks - 1) * K,
        top_block: top,
        top_hash,
        top_pinned_by: pinned,
        genesis_hash: genesis,
        table_keccak256: keccak256(&table),
        source_client: client,
        generated_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    };
    let mut side = opts.out.as_os_str().to_owned();
    side.push(".json");
    std::fs::write(PathBuf::from(side), serde_json::to_vec_pretty(&meta)?)?;
    let _ = std::fs::remove_file(&work);
    Ok(meta)
}

/// Compares `samples` table entries with another node's hashes (an independent client).
pub async fn cross_check(table: &Path, other_rpc: &str, samples: u64) -> Result<u64> {
    let bytes = std::fs::read(table)?;
    ensure!(
        bytes.len() % 32 == 0,
        "{} is not a table of hashes",
        table.display()
    );
    let entries = bytes.len() as u64 / 32;
    let http = reqwest::Client::new();
    let step = (entries / samples.max(1)).max(1);
    let mut checked = 0;
    let mut i = 0;
    while i < entries {
        let n = i * K;
        let h = rpc_call(
            &http,
            other_rpc,
            "eth_getBlockByNumber",
            serde_json::json!([format!("{n:#x}"), false]),
        )
        .await?;
        let theirs: B256 = h["hash"].as_str().context("no hash")?.parse()?;
        let ours = B256::from_slice(&bytes[i as usize * 32..i as usize * 32 + 32]);
        ensure!(
            ours == theirs,
            "block {n}: table {ours}, {other_rpc} {theirs}"
        );
        checked += 1;
        i += step;
    }
    // The last entry always.
    let n = (entries - 1) * K;
    let h = rpc_call(
        &http,
        other_rpc,
        "eth_getBlockByNumber",
        serde_json::json!([format!("{n:#x}"), false]),
    )
    .await?;
    let theirs: B256 = h["hash"].as_str().context("no hash")?.parse()?;
    let ours = B256::from_slice(&bytes[bytes.len() - 32..]);
    ensure!(
        ours == theirs,
        "block {n}: table {ours}, {other_rpc} {theirs}"
    );
    Ok(checked + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_records_round_trip() {
        let c = Chunk {
            parent: B256::repeat_byte(1),
            first: B256::repeat_byte(2),
            last: B256::repeat_byte(3),
        };
        let r = c.encode(42);
        assert_eq!(Chunk::decode(&r), (42, c));
    }
}
