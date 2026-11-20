//! Reading what appearance detection needs from an execution-layer JSON-RPC endpoint.
//!
//! Everything except the traces is verified against the block hash the node reports: the
//! header re-hashes to it, the body matches the header's roots (`chain::fetch_verified_block`),
//! the receipts match `receiptsRoot`, and senders are recovered from the signatures rather
//! than taken from the node. Traces cannot be verified without re-executing the block; they
//! are only checked to belong to this block and its transactions.

use alloy::consensus::Transaction as _;
use alloy::consensus::proofs::calculate_receipt_root;
use alloy::consensus::transaction::SignerRecoverable as _;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::ReceiptResponse as _;
use alloy::providers::Provider;
use alloy::transports::{RpcError, TransportError};
use anyhow::{Context, Result, bail, ensure};

use alloy::consensus::TxEnvelope;
use alloy::primitives::Address;

use std::collections::BTreeMap;

use alloy::primitives::B256;

use super::detect::{
    Appearance, BlockData, LogData, Trace, TxData, check_frames, check_traces, detect,
    detect_with_frames, role,
};
use super::layout::{first_block, last_block};
use super::slice::{BuiltSlice, SliceId, build_slice};
use crate::chain::fetch_verified_block;

/// The sender of a transaction from a verified block. Recovery skips EIP-2's low-`s` rule:
/// before Homestead a high `s` was valid (block 46170 has one), and from Homestead on
/// consensus enforces it, so the block's match with `transactionsRoot` already covers it.
fn sender(tx: &TxEnvelope) -> Result<Address> {
    Ok(tx.recover_signer_unchecked()?)
}

/// First mainnet block of Byzantium (EIP-658). A receipt before it holds the state root after
/// its transaction instead of a status, and the header's receipts root covers that root.
pub const MAINNET_BYZANTIUM: u64 = 4_370_000;

/// The `--receipts-rpc` node, with the block before which it is asked: Byzantium on the
/// chain `--rpc` serves.
pub(crate) struct EarlyReceipts {
    node: alloy::providers::DynProvider,
    before: u64,
}

/// `--receipts-rpc`, process-wide like `--rpc`: set once at start, set up by the first fetch
/// (`prepare_early_receipts`). One `--rpc` per process: the setup checks the node against the
/// first provider that fetches, and serves every later fetch from the same chain.
static EARLY_URL: std::sync::OnceLock<alloy::transports::http::reqwest::Url> =
    std::sync::OnceLock::new();
/// `None`: the chain has no blocks before Byzantium, so the node is never asked.
static EARLY: tokio::sync::OnceCell<Option<EarlyReceipts>> = tokio::sync::OnceCell::const_new();

/// How long connecting to the `--receipts-rpc` node may take, and how often its setup is tried
/// before the run stops: a host that does not answer costs this much once, not per block.
const EARLY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const EARLY_SETUP_ATTEMPTS: u32 = 3;

/// Reads the receipts of the blocks before Byzantium from `url` instead of `--rpc`. Erigon 3
/// answers those receipts with a status and without the state root they hold, so their root
/// cannot be checked against the header from its answer; geth keeps it. Everything else
/// (blocks, traces, later receipts) still comes from `--rpc`, and the receipts from `url` are
/// checked against the header like any others.
pub fn set_receipts_rpc(url: alloy::transports::http::reqwest::Url) -> Result<()> {
    EARLY_URL
        .set(url)
        .map_err(|_| anyhow::anyhow!("--receipts-rpc is set already"))
}

/// Sets up `--receipts-rpc` once, when the first block before mainnet's Byzantium is to be
/// fetched (later blocks never need it, on any chain): asks `--rpc`'s chain, and on a
/// chain without blocks before Byzantium warns and never contacts the node. Otherwise connects
/// (with a connect timeout) and checks that the node serves the same chain, a few attempts in
/// all. Later calls return the result.
pub async fn prepare_early_receipts(
    provider: &impl Provider,
) -> Result<Option<&'static EarlyReceipts>> {
    let Some(url) = EARLY_URL.get() else {
        return Ok(None);
    };
    let early = EARLY
        .get_or_try_init(|| async {
            let main_chain = provider
                .get_chain_id()
                .await
                .context("querying --rpc's chain id")?;
            let before = byzantium_block(main_chain);
            if before == 0 {
                tracing::warn!(
                    "--receipts-rpc has no effect on chain {main_chain}: every receipt there \
                     holds a status. The node is not contacted"
                );
                return Ok(None);
            }
            let client = alloy::transports::http::reqwest::Client::builder()
                .connect_timeout(EARLY_CONNECT_TIMEOUT)
                .build()
                .context("building the --receipts-rpc client")?;
            let node = alloy::providers::ProviderBuilder::new()
                .connect_client(alloy::rpc::client::RpcClient::new(
                    alloy::transports::http::Http::with_client(client, url.clone()),
                    false,
                ))
                .erased();
            let mut attempt = 1;
            let early_chain = loop {
                match node.get_chain_id().await {
                    Ok(chain) => break chain,
                    Err(e) if attempt < EARLY_SETUP_ATTEMPTS => {
                        tracing::warn!(
                            "--receipts-rpc {url}, attempt {attempt}/{EARLY_SETUP_ATTEMPTS}: \
                             {e:#}"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1 << (attempt - 1)))
                            .await;
                        attempt += 1;
                    }
                    Err(e) => {
                        return Err(anyhow::Error::from(e))
                            .with_context(|| format!("querying --receipts-rpc {url}'s chain id"));
                    }
                }
            };
            ensure!(
                main_chain == early_chain,
                "--rpc serves chain {main_chain}, --receipts-rpc chain {early_chain}"
            );
            anyhow::Ok(Some(EarlyReceipts { node, before }))
        })
        .await?;
    Ok(early.as_ref())
}

/// The block before which the chain's receipts hold state roots: Byzantium on mainnet, none
/// elsewhere (the other chains this tool reads started after it).
pub fn byzantium_block(chain_id: u64) -> u64 {
    if chain_id == 1 { MAINNET_BYZANTIUM } else { 0 }
}

/// Whether block `number`'s receipts come from the early node (which serves blocks before
/// `before`).
fn from_early_node(number: u64, before: u64) -> bool {
    number < before
}

/// What to add to a receipts-root mismatch of block `number` (`byzantium`: the chain's): when
/// a receipt before Byzantium came with a status, the node dropped the state roots. From
/// `--rpc` that calls for `--receipts-rpc`; from the `--receipts-rpc` node, for another node.
/// Nothing otherwise: another fault.
fn root_hint<R: alloy::consensus::TxReceipt>(
    number: u64,
    byzantium: u64,
    receipts: &[R],
    from_early: bool,
) -> &'static str {
    let status = receipts.iter().any(|r| {
        !matches!(
            r.status_or_post_state(),
            alloy::consensus::Eip658Value::PostState(_)
        )
    });
    match (number < byzantium && status, from_early) {
        (false, _) => "",
        (true, false) => {
            ". The node answered receipts from before Byzantium with a status instead of the \
             state root they hold (Erigon 3 does): pass --receipts-rpc <a node that keeps it, \
             such as geth>"
        }
        (true, true) => {
            ". The --receipts-rpc node answered with a status as well: it has to be a node \
             that keeps the state root of receipts before Byzantium, such as geth"
        }
    }
}

/// Fetch block `number` with its receipts and uncles, and its `trace_block` traces if
/// `traces` is set (Erigon, Reth, Anvil; not geth). Receipts of a block before Byzantium come
/// from the `--receipts-rpc` node when one is set ([`set_receipts_rpc`]).
pub async fn fetch_block_data(
    provider: &impl Provider,
    number: u64,
    traces: bool,
) -> Result<BlockData> {
    // No chain this tool knows has receipts with state roots from mainnet's Byzantium on, so a
    // later block never needs (or contacts) the --receipts-rpc node.
    let early = if number < MAINNET_BYZANTIUM {
        prepare_early_receipts(provider).await?
    } else {
        None
    };
    fetch_block_data_with(provider, early, number, traces).await
}

/// [`fetch_block_data`] with the early-receipts node given (tests drive it with mocks).
async fn fetch_block_data_with(
    provider: &impl Provider,
    early: Option<&EarlyReceipts>,
    number: u64,
    traces: bool,
) -> Result<BlockData> {
    let block = fetch_verified_block(provider, number).await?;
    let hash = block.hash;
    let early = early.filter(|e| from_early_node(number, e.before));
    let source = if early.is_some() {
        " from --receipts-rpc"
    } else {
        ""
    };

    let receipts_fut = async {
        let receipts = match early {
            Some(e) => e.node.get_block_receipts(BlockId::hash(hash)).await,
            None => provider.get_block_receipts(BlockId::hash(hash)).await,
        };
        receipts
            .with_context(|| format!("querying receipts of block {number}{source}"))?
            .with_context(|| {
                format!(
                    "no receipts for block {number}{source} (does the node keep that block's \
                     history?)"
                )
            })
    };
    let traces_fut = async {
        if !traces {
            return Ok(None);
        }
        let t: Vec<Trace> = provider
            .raw_request("trace_block".into(), (BlockNumberOrTag::Number(number),))
            .await
            .with_context(|| format!("trace_block {number} (does the node support traces?)"))?;
        anyhow::Ok(Some(t))
    };
    let (receipts, traces) = tokio::try_join!(receipts_fut, traces_fut)?;

    let txs = &block.body.transactions;
    ensure!(
        txs.len() == receipts.len(),
        "block {number}: {} transactions but {} receipts",
        txs.len(),
        receipts.len()
    );
    let consensus_receipts: Vec<_> = receipts
        .iter()
        .map(|r| r.inner.clone().into_primitives_receipt())
        .collect();
    let receipts_root = calculate_receipt_root(&consensus_receipts);
    if receipts_root != block.header.receipts_root {
        // The chain's Byzantium block: the early node's, else asked of --rpc, only here on
        // the failure.
        let byzantium = match early {
            Some(e) => e.before,
            None => provider
                .get_chain_id()
                .await
                .map(byzantium_block)
                .unwrap_or(0),
        };
        let hint = root_hint(number, byzantium, &consensus_receipts, early.is_some());
        bail!(
            "block {number}: receipts root {receipts_root}{source} != header {}{hint}",
            block.header.receipts_root
        );
    }

    let tx_data = txs
        .iter()
        .zip(&receipts)
        .enumerate()
        .map(|(i, (tx, receipt))| {
            ensure!(
                receipt.transaction_hash() == *tx.tx_hash(),
                "block {number}: receipt {i} belongs to another transaction"
            );
            let sender = sender(tx)
                .with_context(|| format!("block {number}: recovering the sender of tx {i}"))?;
            Ok(TxData {
                sender,
                to: tx.to(),
                nonce: tx.nonce(),
                input: tx.input().clone(),
                authorizations: if tx.is_eip7702() {
                    tx.authorization_list().unwrap_or_default().to_vec()
                } else {
                    Vec::new()
                },
                logs: receipt
                    .inner
                    .logs()
                    .iter()
                    .map(|log| LogData {
                        address: log.address(),
                        topics: log.topics().to_vec(),
                        data: log.data().data.clone(),
                    })
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let uncle_beneficiaries: Vec<_> = block.body.ommers.iter().map(|u| u.beneficiary).collect();
    if let Some(traces) = &traces {
        let tx_hashes: Vec<_> = txs.iter().map(|tx| *tx.tx_hash()).collect();
        let mut beneficiaries = uncle_beneficiaries.clone();
        beneficiaries.push(block.header.beneficiary);
        check_traces(traces, hash, &tx_hashes, &beneficiaries)
            .with_context(|| format!("block {number}: traces from trace_block"))?;
    }

    Ok(BlockData {
        number,
        hash,
        parent_hash: block.header.parent_hash,
        timestamp: block.header.timestamp,
        beneficiary: block.header.beneficiary,
        uncle_beneficiaries,
        withdrawal_recipients: block
            .body
            .withdrawals
            .as_ref()
            .map(|w| w.iter().map(|w| w.address).collect())
            .unwrap_or_default(),
        txs: tx_data,
        traces,
    })
}

/// Attempts per block before [`fetch_appearances`] gives up.
const ATTEMPTS: u32 = 4;

/// Whether an error is a transport failure (connection, timeout, a body that broke off), the
/// only kind a retry can fix. Error responses, missing methods (no `trace_block`), failed
/// verification and rejected traces fail every attempt alike.
fn is_transport_failure(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<TransportError>(),
            Some(RpcError::Transport(_))
        )
    })
}

/// [`fetch_block_data`], retried with growing pauses (1, 2, 4 s) after a transport failure,
/// so that one dropped connection to a slow node does not lose a window of thousands of
/// blocks. Any other error is returned at once.
async fn fetch_with_retry<P: Provider>(
    provider: &P,
    number: u64,
    traces: bool,
) -> Result<BlockData> {
    let mut attempt = 1;
    loop {
        match fetch_block_data(provider, number, traces).await {
            Ok(data) => return Ok(data),
            Err(e) if attempt < ATTEMPTS && is_transport_failure(&e) => {
                tracing::warn!("block {number}, attempt {attempt}/{ATTEMPTS}: {e:#}");
                tokio::time::sleep(std::time::Duration::from_secs(1 << (attempt - 1))).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Where `TRACE` comes from: nowhere, `trace_block` (Erigon, Reth, Anvil), or an archive
/// node's frame index (`super::trace_index`, exact for the `TRACE` rule of `docs/address-index.md`).
#[derive(Debug, Clone, Default)]
pub enum Traces {
    #[default]
    Off,
    Rpc,
    Index(std::sync::Arc<super::trace_index::TraceIndex>),
}

impl Traces {
    /// From the `--traces` flag (`trace_block` or nothing).
    pub fn rpc(on: bool) -> Self {
        if on { Self::Rpc } else { Self::Off }
    }

    /// Whether `TRACE` is detected at all.
    pub fn detects(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// [`detect`] for a fetched block, `TRACE` from `traces`: an index's entry for the block must
/// name the same block (hash) with the same number of transactions, and hold every
/// transaction's top-level frame ([`check_frames`]).
fn detect_block(data: &BlockData, traces: &Traces) -> Result<Vec<Appearance>> {
    match traces {
        Traces::Index(index) => {
            // A binary search of preads over a file that can be GBs: off the async workers.
            let entry = tokio::task::block_in_place(|| index.block(data.number))?;
            ensure!(
                entry.hash == data.hash,
                "block {}: the trace index has hash {}, the node {} (another chain or a reorg)",
                data.number,
                entry.hash,
                data.hash
            );
            ensure!(
                entry.tx_count as usize == data.txs.len(),
                "block {}: the trace index counts {} transactions, the block has {}",
                data.number,
                entry.tx_count,
                data.txs.len()
            );
            check_frames(data, &entry.frames).with_context(|| format!("block {}", data.number))?;
            Ok(detect_with_frames(data, Some(&entry.frames)))
        }
        _ => Ok(detect(data)),
    }
}

/// The appearances of one block, with the hashes that link it to its neighbours.
#[derive(Debug, Clone)]
pub struct BlockAppearances {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub txs: usize,
    pub appearances: Vec<Appearance>,
}

/// Fetches and detects every block in `blocks`, up to `concurrency` blocks in flight, and
/// hands the results to `sink` in block order. Only the appearances are kept, and at most
/// `2 · concurrency` blocks wait to be handed on. Each block must be the child of the one
/// before it (`parent_hash`), so a range never mixes two forks; the caller checks the first
/// block's parent if it knows it.
pub async fn fetch_appearances<P>(
    provider: &P,
    blocks: std::ops::RangeInclusive<u64>,
    traces: &Traces,
    concurrency: usize,
    mut sink: impl FnMut(BlockAppearances) -> Result<()>,
) -> Result<()>
where
    P: Provider + Clone + 'static,
{
    let concurrency = concurrency.max(1);
    let (mut next_spawn, end) = (*blocks.start(), *blocks.end());
    if next_spawn > end {
        return Ok(());
    }
    // Set up --receipts-rpc once, before the tasks, when the range has a block that may need
    // it: its connect timeout and attempts then apply once, not under every block's retries.
    // A range after Byzantium never contacts it.
    if next_spawn < MAINNET_BYZANTIUM {
        prepare_early_receipts(provider).await?;
    }
    let mut next_emit = next_spawn;
    let mut waiting: BTreeMap<u64, BlockAppearances> = BTreeMap::new();
    let mut prev_hash: Option<B256> = None;
    let mut set = tokio::task::JoinSet::new();
    while next_emit <= end {
        while set.len() < concurrency
            && next_spawn <= end
            && next_spawn - next_emit < 2 * concurrency as u64
        {
            let (p, n, t) = (provider.clone(), next_spawn, traces.clone());
            set.spawn(async move {
                let data = fetch_with_retry(&p, n, matches!(t, Traces::Rpc)).await?;
                let appearances = detect_block(&data, &t)?;
                anyhow::Ok(BlockAppearances {
                    number: n,
                    hash: data.hash,
                    parent_hash: data.parent_hash,
                    timestamp: data.timestamp,
                    txs: data.txs.len(),
                    appearances,
                })
            });
            next_spawn += 1;
        }
        let done = set
            .join_next()
            .await
            .expect("a fetch is in flight while blocks remain")
            .context("block fetch task panicked")??;
        waiting.insert(done.number, done);
        while let Some(b) = waiting.remove(&next_emit) {
            if let Some(prev) = prev_hash {
                ensure!(
                    b.parent_hash == prev,
                    "block {}: parent {} is not block {}'s hash {prev} (reorg or mixed sources?)",
                    b.number,
                    b.parent_hash,
                    b.number - 1
                );
            }
            prev_hash = Some(b.hash);
            next_emit += 1;
            sink(b)?;
        }
    }
    Ok(())
}

/// Fetches the blocks of `slice` (of an index from genesis) and builds it in memory, with the
/// hash of its last block: for `addr-slice`. The importer builds with bounded memory and
/// publishes as it builds (`import`).
pub async fn fetch_slice<P>(
    provider: &P,
    slice: SliceId,
    traces: &Traces,
    concurrency: usize,
) -> Result<BuiltSlice>
where
    P: Provider + Clone + 'static,
{
    let (first, last) = (first_block(slice.0, slice.1), last_block(slice.0, slice.1));
    let mut apps = Vec::new();
    let mut last_hash = B256::ZERO;
    fetch_appearances(provider, first..=last, traces, concurrency, |b| {
        last_hash = b.hash;
        apps.extend(b.appearances);
        Ok(())
    })
    .await?;
    build_slice(slice, 0, &apps, last_hash, role::mask(traces.detects()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_index_must_name_the_fetched_block() {
        use super::super::trace_index::{TraceIndex, tests::write};
        let dir = std::env::temp_dir().join(format!("a1tr-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hash = B256::repeat_byte(5);
        let (sender, to, inner) = (
            Address::repeat_byte(0x22),
            Address::repeat_byte(0x33),
            Address::repeat_byte(0x44),
        );
        write(
            &dir,
            1,
            7,
            8,
            &[(hash, 1), (B256::repeat_byte(8), 1)],
            // Block 8's records lack the top-level frame's recipient.
            &[(7, 0, sender), (7, 0, to), (7, 0, inner), (8, 0, sender)],
        );
        let traces = Traces::Index(std::sync::Arc::new(TraceIndex::open(&dir, 1).unwrap()));
        let tx = TxData {
            sender,
            to: Some(to),
            ..Default::default()
        };
        let data = BlockData {
            number: 7,
            hash,
            txs: vec![tx.clone()],
            ..Default::default()
        };
        let apps = super::detect_block(&data, &traces).unwrap();
        assert!(
            apps.iter()
                .any(|a| a.address == inner && a.roles == role::TRACE)
        );
        // Another hash, another transaction count: refused.
        let other = BlockData {
            hash: B256::repeat_byte(6),
            ..data.clone()
        };
        assert!(super::detect_block(&other, &traces).is_err());
        let more = BlockData {
            txs: vec![tx.clone(), tx],
            ..data.clone()
        };
        assert!(super::detect_block(&more, &traces).is_err());
        // Records cut short: refused, not imported as fewer frames.
        let short = BlockData {
            number: 8,
            hash: B256::repeat_byte(8),
            ..data
        };
        let e = super::detect_block(&short, &traces).unwrap_err();
        assert!(
            format!("{e:#}").contains("no frame for its recipient"),
            "{e:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn early_receipts_and_the_hint() {
        use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
        let receipt = |status: Eip658Value| {
            ReceiptEnvelope::Legacy(ReceiptWithBloom::from(Receipt::<alloy::primitives::Log> {
                status,
                cumulative_gas_used: 21_000,
                logs: vec![],
            }))
        };
        // The last block before Byzantium is the early node's, Byzantium's first is not.
        assert!(from_early_node(MAINNET_BYZANTIUM - 1, MAINNET_BYZANTIUM));
        assert!(!from_early_node(MAINNET_BYZANTIUM, MAINNET_BYZANTIUM));
        assert!(!from_early_node(0, 0), "a chain without such blocks");
        let roots = [receipt(Eip658Value::PostState(B256::repeat_byte(1)))];
        let status = [receipt(Eip658Value::Eip658(true))];
        let mixed = [roots[0].clone(), status[0].clone()];
        let b = MAINNET_BYZANTIUM;
        // A status before Byzantium: from --rpc, pass --receipts-rpc; from that node, another.
        assert!(root_hint(999_425, b, &status, false).contains("pass --receipts-rpc"));
        assert!(root_hint(999_425, b, &mixed, false).contains("pass --receipts-rpc"));
        assert!(root_hint(999_425, b, &status, true).contains("answered with a status as well"));
        // State roots (another fault), after Byzantium, or a chain without such blocks: none.
        assert_eq!(root_hint(999_425, b, &roots, false), "");
        assert_eq!(root_hint(b, b, &status, false), "");
        assert_eq!(root_hint(5, 0, &status, true), "");
    }

    /// Block 999,425 of mainnet (one transaction), as nuc's Erigon serves it, with its
    /// receipts from that Erigon (a status, no state root) and from zbox's geth (the root).
    fn fixture(name: &str) -> serde_json::Value {
        let path = format!(
            "{}/testdata/early-receipts/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn mocked(responses: &[serde_json::Value]) -> alloy::providers::DynProvider {
        let asserter = alloy::transports::mock::Asserter::new();
        for r in responses {
            asserter.push_success(r);
        }
        alloy::providers::ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased()
    }

    #[tokio::test]
    async fn receipts_before_byzantium_come_from_the_early_node() {
        let block = fixture("block-999425.json");
        let (erigon, geth) = (
            fixture("receipts-999425-erigon.json"),
            fixture("receipts-999425-geth.json"),
        );
        let early = |answer: &serde_json::Value| EarlyReceipts {
            node: mocked(std::slice::from_ref(answer)),
            before: MAINNET_BYZANTIUM,
        };
        // --rpc gives the block, the early node geth's receipts: verified.
        let main = mocked(std::slice::from_ref(&block));
        let data = fetch_block_data_with(&main, Some(&early(&geth)), 999_425, false)
            .await
            .unwrap();
        assert_eq!((data.number, data.txs.len()), (999_425, 1));
        // From --rpc alone (Erigon's receipts, then the chain asked on the failure): the hint.
        let main = mocked(&[block.clone(), erigon.clone(), serde_json::json!("0x1")]);
        let e = format!(
            "{:#}",
            fetch_block_data_with(&main, None, 999_425, false)
                .await
                .unwrap_err()
        );
        assert!(
            e.contains("pass --receipts-rpc") && !e.contains("from --receipts-rpc"),
            "{e}"
        );
        // The early node drops the roots too: named, and told to be another node.
        let main = mocked(std::slice::from_ref(&block));
        let e = format!(
            "{:#}",
            fetch_block_data_with(&main, Some(&early(&erigon)), 999_425, false)
                .await
                .unwrap_err()
        );
        assert!(
            e.contains("from --receipts-rpc") && e.contains("answered with a status as well"),
            "{e}"
        );
        // At or after the boundary the early node is not asked: --rpc's receipts are used.
        let unasked = EarlyReceipts {
            node: mocked(&[]),
            before: 999_425,
        };
        let main = mocked(&[block, geth]);
        assert!(
            fetch_block_data_with(&main, Some(&unasked), 999_425, false)
                .await
                .is_ok()
        );
    }

    #[test]
    fn byzantium_only_on_mainnet() {
        assert_eq!(byzantium_block(1), 4_370_000);
        // Sepolia, Gnosis, the dev chains: receipts with a status from their first block.
        for chain in [11_155_111, 100, 1337, 31_337] {
            assert_eq!(byzantium_block(chain), 0, "chain {chain}");
        }
    }

    #[test]
    fn only_transport_failures_are_retried() {
        let local: anyhow::Error = anyhow::Error::from(TransportError::<
            Box<serde_json::value::RawValue>,
        >::UnsupportedFeature("x"));
        assert!(
            !is_transport_failure(&local),
            "a local error is not transport"
        );
        let dropped: anyhow::Error = TransportError::<Box<serde_json::value::RawValue>>::Transport(
            alloy::transports::TransportErrorKind::BackendGone,
        )
        .into();
        assert!(is_transport_failure(&dropped.context("querying block 7")));
        // A non-transport RPC error (an error response, here a null one) is not retried.
        let resp =
            anyhow::Error::from(TransportError::<Box<serde_json::value::RawValue>>::NullResp);
        assert!(!is_transport_failure(&resp));
        assert!(!is_transport_failure(&anyhow::anyhow!(
            "receipts root mismatch"
        )));
    }
    use alloy::consensus::{SignableTransaction, Signed, TxLegacy};
    use alloy::primitives::{Signature, TxKind, U256};
    use alloy::signers::SignerSync;
    use alloy::signers::local::PrivateKeySigner;

    #[test]
    fn sender_accepts_a_high_s_signature() {
        let key = PrivateKeySigner::random();
        let tx = TxLegacy {
            nonce: 1,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::repeat_byte(0x11)),
            ..Default::default()
        };
        let sig = key.sign_hash_sync(&tx.signature_hash()).unwrap();
        // The other valid signature of the same message: s' = n − s, parity flipped. Pre-EIP-2
        // transactions may carry it.
        let n = U256::from_be_bytes(alloy::hex!(
            "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
        ));
        let high = Signature::new(sig.r(), n - sig.s(), !sig.v());
        let env = TxEnvelope::Legacy(Signed::new_unhashed(tx, high));
        assert!(env.recover_signer().is_err(), "the EIP-2 check rejects it");
        assert_eq!(sender(&env).unwrap(), key.address());
    }
}
