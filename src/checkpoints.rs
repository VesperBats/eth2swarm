//! `ethswarm checkpoint-table`: the hash of every 1024th block, from genesis up to a finalized
//! block, for Myotis's deep-block verification (its `docs/ideas/deep-block-verification.md`,
//! decision D1: a bundled table, owner's decision 2026-11-20).
//!
//! The node is asked for headers only, and none of the hashes it reports is used: every header
//! is RLP-encoded and hashed here, each one's `parentHash` must be the hash computed for the
//! block below it, and the walk must end at the network's genesis hash. That makes the table
//! one chain, but not the canonical one: linking back to genesis authenticates nothing on its
//! own (no proof of work or beacon check is done here), so a node could serve a made-up fork
//! from block 1 on. What makes it the canonical chain is the top: its hash must come from
//! another source than the node, `--top-hash` (for example a beacon-attested finalized block,
//! as Myotis's light client gives it) or `--cross-check` (another client's hashes, at about 200
//! entries and the top). A run with neither is refused.
//!
//! The work is done in chunks of 1024 blocks, fetched in parallel; each finished chunk's
//! summary (its first block's parent hash, its first and last block's hashes) is appended to a
//! work file, so a run that stops resumes where it was, with the same top. Once every chunk is
//! in, the chunks are linked to each other, and the table is written: entry `i` is the hash of
//! block `i·1024`.

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
        by_number.len() as u64 == to - from + 1
            && by_number.keys().next() == Some(&from)
            && by_number.keys().next_back() == Some(&to),
        "asked for headers {from}..={to}, got {} answers for {:?}..={:?}",
        by_number.len(),
        by_number.keys().next(),
        by_number.keys().next_back()
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
    /// Whether a cross-check against another node follows (`--cross-check`), which pins the
    /// top in place of `top_hash`.
    pub cross_check: bool,
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
    suffixed(out, ".work")
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
    // The work file of an unfinished run: its tag names chain, genesis and top.
    let work = work_path(&opts.out);
    let saved = match std::fs::read(&work) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", work.display())),
    };
    let saved_top = saved
        .as_deref()
        .and_then(parse_tag)
        .map(|t| (t.top, t.top_hash));
    let (top, top_hash, pinned) = match (opts.top, opts.top_hash, saved_top) {
        (Some(t), Some(h), _) => (t, h, "the caller (--top-hash)".to_string()),
        (Some(_), None, _) | (None, Some(_), _) => bail!("--top and --top-hash go together"),
        // Resuming without --top: the unfinished run's top (the finalized block moves).
        (None, None, Some((t, h))) => (t, h, format!("an earlier run's top ({client})")),
        (None, None, None) => {
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
    ensure!(
        opts.top_hash.is_some() || opts.cross_check,
        "the top block's hash would rest on the node alone, which a walk to genesis cannot \
         authenticate: pass --top-hash (a beacon-attested block) or --cross-check <another node>"
    );
    let chunks = top / K + 1;

    let tag = Tag {
        chain_id,
        genesis,
        top,
        top_hash,
    };
    let mut done: BTreeMap<u64, Chunk> = BTreeMap::new();
    match saved {
        Some(bytes) => {
            let saved_tag = parse_tag(&bytes).with_context(|| {
                format!(
                    "{} has no tag line: remove it to start over",
                    work.display()
                )
            })?;
            ensure!(
                saved_tag == tag,
                "{} is the work of another run ({saved_tag:?}, this run {tag:?}): remove it to \
                 start over, or pass its top",
                work.display()
            );
            let head = tag.line().len();
            let (records, cut) = bytes[head..].as_chunks::<WORK_RECORD>();
            for r in records {
                let (i, c) = Chunk::decode(r);
                ensure!(
                    i < chunks,
                    "{}: a record for chunk {i}, beyond the {chunks} chunks of top {top}: \
                     remove it to start over",
                    work.display()
                );
                done.insert(i, c);
            }
            // A record a stop cut short is dropped, and the file cut back to whole records,
            // so the records appended next stay aligned.
            if !cut.is_empty() {
                let f = std::fs::OpenOptions::new().write(true).open(&work)?;
                f.set_len((head + records.len() * WORK_RECORD) as u64)?;
                f.sync_all()?;
            }
            tracing::info!(done = done.len(), chunks, "resuming");
        }
        None => std::fs::write(&work, tag.line())?,
    }
    let mut work_file = std::fs::OpenOptions::new().append(true).open(&work)?;

    // Newest first: the header encodings of the latest forks are checked within minutes, not
    // at the end of the run.
    let todo: Vec<u64> = (0..chunks)
        .rev()
        .filter(|i| !done.contains_key(i))
        .collect();
    let started = Instant::now();
    let mut last_report = Instant::now();
    let (total, mut finished) = (todo.len() as u64, 0u64);
    let mut queue = todo.into_iter();
    let mut set = tokio::task::JoinSet::new();
    let concurrency = opts.concurrency.max(1);
    let mut failed = None;
    loop {
        // After a failure no new chunk starts, and the running ones are drained, so that what
        // they finished is kept for the resume.
        while failed.is_none() && set.len() < concurrency {
            let Some(i) = queue.next() else { break };
            let (http, rpc) = (http.clone(), opts.rpc.clone());
            set.spawn(async move { (i, fetch_chunk(&http, &rpc, i, top).await) });
        }
        let Some(joined) = set.join_next().await else {
            break;
        };
        let (i, chunk) = joined.context("a chunk task panicked")?;
        match chunk {
            Ok(chunk) => {
                work_file.write_all(&chunk.encode(i))?;
                done.insert(i, chunk);
                finished += 1;
            }
            Err(e) => {
                if failed.is_none() {
                    failed = Some(e.context(format!("chunk {i} (blocks from {})", i * K)));
                }
                continue;
            }
        }
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
    if let Some(e) = failed {
        return Err(e.context(format!(
            "stopped; {} of {chunks} chunks are kept in {} for the next run",
            done.len(),
            work.display()
        )));
    }

    let table = link(&done, chunks, genesis, top, top_hash)?;
    let tmp = suffixed(&opts.out, ".tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&table)?;
        f.sync_all()?;
    }
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
    std::fs::write(
        suffixed(&opts.out, ".json"),
        serde_json::to_vec_pretty(&meta)?,
    )?;
    let _ = std::fs::remove_file(&work);
    Ok(meta)
}

/// `path` with `suffix` appended to its name (not replacing an extension).
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

/// The first line of a work file: the run it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tag {
    chain_id: u64,
    genesis: B256,
    top: u64,
    top_hash: B256,
}

impl Tag {
    fn line(&self) -> String {
        format!(
            "chain {} genesis {} top {} {}\n",
            self.chain_id, self.genesis, self.top, self.top_hash
        )
    }
}

fn parse_tag(bytes: &[u8]) -> Option<Tag> {
    let end = bytes.iter().position(|b| *b == b'\n')?;
    let line = std::str::from_utf8(&bytes[..end]).ok()?;
    let w: Vec<&str> = line.split(' ').collect();
    match w.as_slice() {
        ["chain", c, "genesis", g, "top", t, h] => Some(Tag {
            chain_id: c.parse().ok()?,
            genesis: g.parse().ok()?,
            top: t.parse().ok()?,
            top_hash: h.parse().ok()?,
        }),
        _ => None,
    }
}

/// Links the chunks into one chain from `genesis` to `top_hash` and returns the table: every
/// chunk's first parent is the last hash of the chunk below, block 0 is the genesis, and the
/// top chunk's last block is the pinned top.
fn link(
    done: &BTreeMap<u64, Chunk>,
    chunks: u64,
    genesis: B256,
    top: u64,
    top_hash: B256,
) -> Result<Vec<u8>> {
    ensure!(
        done.len() as u64 == chunks && done.keys().next_back() == Some(&(chunks - 1)),
        "{} of {chunks} chunks",
        done.len()
    );
    ensure!(
        done[&0].first == genesis,
        "block 0 hashes to {}, the genesis hash is {genesis}",
        done[&0].first
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
    Ok(table)
}

/// Compares about `samples` table entries, and always the last, with another node's hashes
/// (another client). Returns how many entries were compared.
pub async fn cross_check(table: &Path, other_rpc: &str, samples: u64) -> Result<u64> {
    let bytes = std::fs::read(table)?;
    ensure!(
        !bytes.is_empty() && bytes.len() % 32 == 0,
        "{} is not a table of hashes",
        table.display()
    );
    let entries = bytes.len() as u64 / 32;
    let http = reqwest::Client::new();
    let step = (entries / samples.max(1)).max(1);
    let mut picks: Vec<u64> = (0..entries).step_by(step as usize).collect();
    if picks.last() != Some(&(entries - 1)) {
        picks.push(entries - 1);
    }
    for &i in &picks {
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
    }
    Ok(picks.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(n: u64) -> BTreeMap<u64, Chunk> {
        // Chunk i's blocks hash to i·3+1 (first) and i·3+2 (last); chunk i's parent is the last
        // of chunk i−1.
        let h = |x: u64| B256::left_padding_from(&x.to_be_bytes());
        (0..n)
            .map(|i| {
                let parent = if i == 0 {
                    B256::ZERO
                } else {
                    h((i - 1) * 3 + 2)
                };
                (
                    i,
                    Chunk {
                        parent,
                        first: h(i * 3 + 1),
                        last: h(i * 3 + 2),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn linking_checks_genesis_every_link_and_the_top() {
        let h = |x: u64| B256::left_padding_from(&x.to_be_bytes());
        let (genesis, top) = (h(1), h(3 * 4 + 2));
        let good = chain(5);
        let table = link(&good, 5, genesis, 4100, top).unwrap();
        assert_eq!(table.len(), 5 * 32);
        assert_eq!(
            &table[32..64],
            h(4).as_slice(),
            "entry 1 = chunk 1's first block"
        );
        // Another genesis, another top, a broken link, a missing chunk: refused.
        assert!(link(&good, 5, h(9), 4100, top).is_err());
        assert!(link(&good, 5, genesis, 4100, h(99)).is_err());
        let mut broken = good.clone();
        broken.get_mut(&3).unwrap().parent = h(77);
        let e = link(&broken, 5, genesis, 4100, top)
            .unwrap_err()
            .to_string();
        assert!(e.contains("block 3072"), "{e}");
        let mut missing = good.clone();
        missing.remove(&2);
        assert!(link(&missing, 5, genesis, 4100, top).is_err());
    }

    #[test]
    fn work_file_tags() {
        let t = Tag {
            chain_id: 1,
            genesis: MAINNET_GENESIS,
            top: 26_457_537,
            top_hash: B256::repeat_byte(7),
        };
        let mut bytes = t.line().into_bytes();
        bytes.extend_from_slice(&[1, 2, 3]);
        assert_eq!(parse_tag(&bytes), Some(t));
        assert_eq!(parse_tag(b"top 5 0x00\n"), None, "an old or foreign line");
    }

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
