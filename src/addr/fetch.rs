//! Reading what appearance detection needs from an execution-layer JSON-RPC endpoint.

use alloy::consensus::Transaction as _;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::{ReceiptResponse as _, TransactionResponse as _};
use alloy::providers::Provider;
use anyhow::{Context, Result, ensure};

use super::detect::{BlockData, LogData, Trace, TxData};

/// Fetch block `number` with its receipts and uncles, and its `trace_block` traces if
/// `traces` is set (Erigon, Reth, Anvil; not geth).
pub async fn fetch_block_data(
    provider: &impl Provider,
    number: u64,
    traces: bool,
) -> Result<BlockData> {
    let block = provider
        .get_block_by_number(number.into())
        .full()
        .await
        .with_context(|| format!("querying block {number}"))?
        .with_context(|| format!("block {number} not found"))?;
    let receipts = provider
        .get_block_receipts(BlockId::number(number))
        .await
        .with_context(|| format!("querying receipts of block {number}"))?
        .with_context(|| format!("no receipts for block {number}"))?;
    let txs = block.transactions.as_transactions().unwrap_or_default();
    ensure!(
        txs.len() == receipts.len(),
        "block {number}: {} transactions but {} receipts",
        txs.len(),
        receipts.len()
    );

    let mut uncle_beneficiaries = Vec::with_capacity(block.uncles.len());
    for idx in 0..block.uncles.len() {
        let uncle = provider
            .get_uncle(BlockId::number(number), idx as u64)
            .await
            .with_context(|| format!("querying uncle {idx} of block {number}"))?
            .with_context(|| format!("uncle {idx} of block {number} not found"))?;
        uncle_beneficiaries.push(uncle.header.beneficiary);
    }

    let txs = txs
        .iter()
        .zip(&receipts)
        .enumerate()
        .map(|(i, (tx, receipt))| {
            ensure!(
                receipt.transaction_hash() == tx.tx_hash(),
                "block {number}: receipt {i} belongs to another transaction"
            );
            Ok(TxData {
                sender: tx.from(),
                to: tx.to(),
                nonce: tx.nonce(),
                input: tx.input().clone(),
                authorizations: tx.authorization_list().unwrap_or_default().to_vec(),
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

    let traces = if traces {
        let t: Vec<Trace> = provider
            .raw_request("trace_block".into(), (BlockNumberOrTag::Number(number),))
            .await
            .with_context(|| format!("trace_block {number} (does the node support traces?)"))?;
        Some(t)
    } else {
        None
    };

    Ok(BlockData {
        number,
        beneficiary: block.header.beneficiary,
        uncle_beneficiaries,
        withdrawal_recipients: block
            .withdrawals
            .as_ref()
            .map(|w| w.iter().map(|w| w.address).collect())
            .unwrap_or_default(),
        txs,
        traces,
    })
}
