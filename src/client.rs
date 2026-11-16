//! Light client: Swarm → verified block data, trusting only the owner address and the anchor.

use crate::accumulator::{self, root_from_proof};
use crate::chain::{Body, check_body};
use crate::layout::{self, AnchorRecord, EpochRecord, Geometry, HeaderRecord};
use crate::swarm::{bee::Bee, soc};
use alloy::consensus::Header;
use alloy::primitives::{Address, B256, keccak256};
use alloy::rlp::Decodable;
use anyhow::{Context, Result, ensure};

/// Upper bound for a downloaded block body (mainnet bodies are well below this).
const MAX_BODY_BYTES: usize = 16 << 20;
/// Upper bound per header in an epoch file (mainnet headers are ≤ ~700 bytes).
const MAX_HEADER_BYTES: usize = 1024;

pub struct Client<'a> {
    pub bee: &'a Bee,
    pub owner: Address,
    pub chain_id: u64,
}

/// A header (and optionally its body) that has been verified against the anchor.
pub struct VerifiedBlock {
    pub hash: B256,
    pub header: Header,
    pub header_rlp_len: usize,
    pub body: Body,
    pub soc_payload_len: usize,
}

impl Client<'_> {
    async fn fetch_soc(&self, id: B256) -> Result<Vec<u8>> {
        let addr = soc::soc_address(&id, &self.owner);
        let raw = self
            .bee
            .get_chunk(&addr)
            .await
            .with_context(|| format!("fetching SOC {addr}"))?;
        soc::verify(&raw, &addr, &self.owner)
    }

    /// The anchor as published by the owner. Only use this when no anchor is pinned:
    /// it means trusting the publisher instead of verifying against an independent root.
    pub async fn published_anchor(&self) -> Result<AnchorRecord> {
        AnchorRecord::decode(&self.fetch_soc(layout::anchor_id(self.chain_id)).await?)
    }

    /// Fetch and verify block `number` (header via accumulator proof, body via header roots).
    pub async fn get_block(&self, number: u64, anchor: B256) -> Result<VerifiedBlock> {
        let payload = self
            .fetch_soc(layout::header_id(self.chain_id, &anchor, number))
            .await?;
        let rec = HeaderRecord::decode(&payload)?;

        let hash = keccak256(&rec.header_rlp);
        let header = Header::decode(&mut rec.header_rlp.as_slice()).context("decoding header")?;
        ensure!(
            header.number == number,
            "SOC for #{number} holds header #{}",
            header.number
        );

        let (epoch, index) = locate(rec.geometry, number)?;
        ensure!(
            rec.epoch_proof.len()
                == accumulator::depth_for(rec.geometry.epoch_size as usize) as usize,
            "epoch proof has wrong length"
        );
        let epoch_root = root_from_proof(hash, index, &rec.epoch_proof);
        check_master(rec.geometry, epoch_root, epoch, &rec.master_proof, anchor)
            .with_context(|| format!("header #{number} ({hash})"))?;

        let body_rlp = self
            .bee
            .get_bytes(&rec.body_ref, MAX_BODY_BYTES)
            .await
            .context("fetching body")?;
        let mut buf = body_rlp.as_slice();
        let body = Body::decode(&mut buf).context("decoding body")?;
        ensure!(buf.is_empty(), "{} trailing bytes after body", buf.len());
        check_body(&header, &body).with_context(|| format!("body of #{number}"))?;

        Ok(VerifiedBlock {
            hash,
            header_rlp_len: rec.header_rlp.len(),
            header,
            body,
            soc_payload_len: payload.len(),
        })
    }

    /// Download a whole epoch's header file and verify every header in it against the anchor
    /// (hash-chain linkage + recomputed epoch root). Returns the verified headers.
    pub async fn verify_epoch(&self, epoch: u64, anchor: B256) -> Result<Vec<Header>> {
        let rec = EpochRecord::decode(
            &self
                .fetch_soc(layout::epoch_id(self.chain_id, &anchor, epoch))
                .await?,
        )?;
        let size = u64::from(rec.geometry.epoch_size);
        let first = epoch
            .checked_mul(size)
            .context("epoch number overflows the block range")?;
        let (located, _) = locate(rec.geometry, first)?;
        check_master(
            rec.geometry,
            rec.epoch_root,
            located,
            &rec.master_proof,
            anchor,
        )?;

        let max = (size as usize).saturating_mul(MAX_HEADER_BYTES);
        let file = self.bee.get_bytes(&rec.headers_ref, max).await?;
        let mut buf = file.as_slice();
        let mut headers = Vec::with_capacity(rec.geometry.epoch_size as usize);
        let mut hashes = Vec::with_capacity(rec.geometry.epoch_size as usize);
        while !buf.is_empty() {
            let start = buf;
            let header = Header::decode(&mut buf).context("decoding header in epoch file")?;
            hashes.push(keccak256(&start[..start.len() - buf.len()]));
            headers.push(header);
        }
        ensure!(
            headers.len() == rec.geometry.epoch_size as usize,
            "epoch file holds {} headers, expected {}",
            headers.len(),
            rec.geometry.epoch_size
        );
        for (i, h) in headers.iter().enumerate() {
            ensure!(
                h.number == first + i as u64,
                "header {i} has number {}",
                h.number
            );
            if i > 0 {
                ensure!(
                    h.parent_hash == hashes[i - 1],
                    "broken hash chain at #{}",
                    h.number
                );
            }
        }
        let tree = accumulator::Tree::new(
            &hashes,
            accumulator::depth_for(rec.geometry.epoch_size as usize),
        );
        ensure!(
            tree.root() == rec.epoch_root,
            "headers hash to epoch root {}, record says {}",
            tree.root(),
            rec.epoch_root
        );
        Ok(headers)
    }
}

/// Epoch index and position within the epoch for `number`.
fn locate(g: Geometry, number: u64) -> Result<(usize, usize)> {
    ensure!(
        g.epoch_size.is_power_of_two(),
        "invalid epoch size {}",
        g.epoch_size
    );
    let size = u64::from(g.epoch_size);
    let epoch = number / size;
    ensure!(
        epoch < u64::from(g.num_epochs),
        "block #{number} is outside the accumulator ({} epochs × {size})",
        g.num_epochs
    );
    Ok((epoch as usize, (number % size) as usize))
}

/// Verify `epoch_root` at position `epoch` of the master tree and that the resulting root,
/// mixed with the claimed geometry, equals the trusted `anchor`.
fn check_master(
    g: Geometry,
    epoch_root: B256,
    epoch: usize,
    master_proof: &[B256],
    anchor: B256,
) -> Result<()> {
    ensure!(
        master_proof.len() == accumulator::depth_for(g.num_epochs as usize) as usize,
        "master proof has wrong length"
    );
    let master_root = root_from_proof(epoch_root, epoch, master_proof);
    let derived = accumulator::anchor(master_root, g.num_epochs.into(), g.epoch_size.into());
    ensure!(
        derived == anchor,
        "proof leads to anchor {derived}, trusted anchor is {anchor}"
    );
    Ok(())
}
