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
use anyhow::{Context, Result, ensure};

use alloy::consensus::TxEnvelope;
use alloy::primitives::Address;

use super::detect::{BlockData, LogData, Trace, TxData, check_traces};
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

/// [`fetch_block_data`] for every block in `blocks`, with up to `concurrency` blocks in
/// flight, returned in block order.
pub async fn fetch_range<P>(
    provider: &P,
    blocks: std::ops::RangeInclusive<u64>,
    traces: bool,
    concurrency: usize,
) -> Result<Vec<BlockData>>
where
    P: Provider + Clone + 'static,
{
    let mut out: Vec<Option<BlockData>> = Vec::new();
    out.resize_with(blocks.clone().count(), || None);
    let first = *blocks.start();
    let mut set = tokio::task::JoinSet::new();
    let mut next = blocks.into_iter();
    loop {
        while set.len() < concurrency.max(1)
            && let Some(n) = next.next()
        {
            let p = provider.clone();
            set.spawn(async move { (n, fetch_block_data(&p, n, traces).await) });
        }
        let Some(done) = set.join_next().await else {
            break;
        };
        let (n, data) = done.context("block fetch task panicked")?;
        out[(n - first) as usize] = Some(data?);
    }
    Ok(out
        .into_iter()
        .map(|d| d.expect("every block fetched"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
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
