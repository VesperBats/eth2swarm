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
use anyhow::{Context, Result, bail, ensure};

use super::detect::{BlockData, LogData, Trace, TxData};
use crate::chain::fetch_verified_block;

/// Trace types detection understands; any other type fails the block, so that an importer
/// never silently disagrees with one that knows the type.
const TRACE_KINDS: [&str; 4] = ["call", "create", "suicide", "reward"];

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
            let sender = tx
                .recover_signer()
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

    if let Some(traces) = &traces {
        for (k, t) in traces.iter().enumerate() {
            if !TRACE_KINDS.contains(&t.kind.as_str()) {
                bail!("block {number}: trace {k} has unknown type {:?}", t.kind);
            }
            if let Some(h) = t.block_hash {
                ensure!(
                    h == hash,
                    "block {number}: trace {k} is for block {h}, not {hash}"
                );
            }
            if let Some(pos) = t.transaction_position {
                let tx = txs.get(pos as usize).with_context(|| {
                    format!(
                        "block {number}: trace {k} names transaction {pos} of {}",
                        txs.len()
                    )
                })?;
                if let Some(h) = t.transaction_hash {
                    ensure!(
                        h == *tx.tx_hash(),
                        "block {number}: trace {k} names transaction {pos} with hash {h}"
                    );
                }
            }
        }
    }

    Ok(BlockData {
        number,
        beneficiary: block.header.beneficiary,
        uncle_beneficiaries: block.body.ommers.iter().map(|u| u.beneficiary).collect(),
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
