//! Reading canonical block data from an execution-layer JSON-RPC endpoint.

use alloy::consensus::{BlockBody, Header, TxEnvelope};
use alloy::eips::BlockId;
use alloy::primitives::{B256, keccak256};
use alloy::providers::Provider;
use anyhow::{Context, Result, ensure};

pub type Body = BlockBody<TxEnvelope>;

/// A block in canonical (hashable) encodings.
pub struct CanonicalBlock {
    pub number: u64,
    pub hash: B256,
    pub header: Header,
    pub header_rlp: Vec<u8>,
    pub body_rlp: Vec<u8>,
}

/// A block whose header hashes to its block hash and whose body matches the header's
/// transactions, ommers and withdrawals roots.
pub struct VerifiedBlock {
    pub hash: B256,
    pub header: Header,
    pub body: Body,
}

/// Fetch block `number` and its uncles, and verify them against the block hash the node
/// reports. Uncles are requested by that hash, so a reorg between the requests fails the
/// check instead of mixing two blocks.
pub async fn fetch_verified_block(provider: &impl Provider, number: u64) -> Result<VerifiedBlock> {
    let block = provider
        .get_block_by_number(number.into())
        .full()
        .await
        .with_context(|| format!("querying block {number}"))?
        .with_context(|| format!("block {number} not found"))?;

    let rpc_hash = block.header.hash;
    let header = block.header.inner.clone();
    let rehashed = keccak256(alloy::rlp::encode(&header));
    ensure!(
        rehashed == rpc_hash,
        "block {number}: re-encoded header hashes to {rehashed}, node says {rpc_hash}"
    );

    let mut ommers = Vec::with_capacity(block.uncles.len());
    for idx in 0..block.uncles.len() {
        let uncle = provider
            .get_uncle(BlockId::hash(rpc_hash), idx as u64)
            .await
            .with_context(|| format!("querying uncle {idx} of block {number}"))?
            .with_context(|| format!("uncle {idx} of block {number} not found"))?;
        ommers.push(uncle.header.inner);
    }

    let body = Body {
        transactions: block
            .transactions
            .into_transactions_vec()
            .into_iter()
            .map(|tx| tx.into_inner())
            .collect(),
        ommers,
        withdrawals: block.withdrawals,
    };
    check_body(&header, &body).with_context(|| format!("block {number}: body from RPC"))?;

    Ok(VerifiedBlock {
        hash: rpc_hash,
        header,
        body,
    })
}

/// Fetch block `number` and re-encode header and body canonically (see
/// [`fetch_verified_block`] for the checks).
pub async fn fetch_block(provider: &impl Provider, number: u64) -> Result<CanonicalBlock> {
    let b = fetch_verified_block(provider, number).await?;
    Ok(CanonicalBlock {
        number,
        hash: b.hash,
        header_rlp: alloy::rlp::encode(&b.header),
        body_rlp: alloy::rlp::encode(&b.body),
        header: b.header,
    })
}

/// Check that `body` matches the commitments in `header`.
pub fn check_body(header: &Header, body: &Body) -> Result<()> {
    let tx_root = alloy::consensus::proofs::calculate_transaction_root(&body.transactions);
    ensure!(
        tx_root == header.transactions_root,
        "transactions root {tx_root} != header {}",
        header.transactions_root
    );
    let ommers_root = body.calculate_ommers_root();
    ensure!(
        ommers_root == header.ommers_hash,
        "ommers hash {ommers_root} != header {}",
        header.ommers_hash
    );
    let withdrawals_root = body.calculate_withdrawals_root();
    ensure!(
        withdrawals_root == header.withdrawals_root,
        "withdrawals root {withdrawals_root:?} != header {:?}",
        header.withdrawals_root
    );
    Ok(())
}

/// Number of fields in the header's RLP list — identifies the fork "shape" of the header
/// (15 = pre-London, 16 = London, 17 = Shanghai, 20 = Cancun, 21 = Prague).
pub fn header_field_count(header: &Header) -> usize {
    15 + [
        header.base_fee_per_gas.is_some(),
        header.withdrawals_root.is_some(),
        header.blob_gas_used.is_some(),
        header.excess_blob_gas.is_some(),
        header.parent_beacon_block_root.is_some(),
        header.requests_hash.is_some(),
    ]
    .iter()
    .filter(|&&present| present)
    .count()
}
