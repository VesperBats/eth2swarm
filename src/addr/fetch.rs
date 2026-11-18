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
use anyhow::{Context, Result, ensure};

use alloy::consensus::TxEnvelope;
use alloy::primitives::Address;

use std::collections::BTreeMap;

use alloy::primitives::B256;

use super::detect::{Appearance, BlockData, LogData, Trace, TxData, check_traces, detect};
use crate::chain::fetch_verified_block;

/// The sender of a transaction from a verified block. Recovery skips EIP-2's low-`s` rule:
/// before Homestead a high `s` was valid (block 46170 has one), and from Homestead on
/// consensus enforces it, so the block's match with `transactionsRoot` already covers it.
fn sender(tx: &TxEnvelope) -> Result<Address> {
    Ok(tx.recover_signer_unchecked()?)
}

/// Fetch block `number` with its receipts and uncles, and its `trace_block` traces if
/// `traces` is set (Erigon, Reth, Anvil; not geth).
pub async fn fetch_block_data(
    provider: &impl Provider,
    number: u64,
    traces: bool,
) -> Result<BlockData> {
    let block = fetch_verified_block(provider, number).await?;
    let hash = block.hash;

    let receipts_fut = async {
        provider
            .get_block_receipts(BlockId::hash(hash))
            .await
            .with_context(|| format!("querying receipts of block {number}"))?
            .with_context(|| format!("no receipts for block {number}"))
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
    ensure!(
        receipts_root == block.header.receipts_root,
        "block {number}: receipts root {receipts_root} != header {}",
        block.header.receipts_root
    );

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
    traces: bool,
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
    let mut next_emit = next_spawn;
    let mut waiting: BTreeMap<u64, BlockAppearances> = BTreeMap::new();
    let mut prev_hash: Option<B256> = None;
    let mut set = tokio::task::JoinSet::new();
    while next_emit <= end {
        while set.len() < concurrency
            && next_spawn <= end
            && next_spawn - next_emit < 2 * concurrency as u64
        {
            let (p, n) = (provider.clone(), next_spawn);
            set.spawn(async move {
                let data = fetch_with_retry(&p, n, traces).await?;
                anyhow::Ok(BlockAppearances {
                    number: n,
                    hash: data.hash,
                    parent_hash: data.parent_hash,
                    timestamp: data.timestamp,
                    txs: data.txs.len(),
                    appearances: detect(&data),
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

#[cfg(test)]
mod tests {
    use super::*;

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
