//! Reading the published address index back from Swarm, and `addr-verify`: a full check of
//! what one head makes a client read.

use alloy::primitives::{Address, B256, keccak256};
use anyhow::{Context, Result, ensure};

use super::layout::{
    AddressRecord, BucketBody, BucketRecord, Entries, HeadRecord, SliceRecord, bucket_id,
    decode_bucket_page, decode_spill_page, first_block, head_id, last_block, origin_id, pages,
    slice_id,
};
use super::slice::{SliceId, resolve};
use crate::accumulator::{Tree, depth_for};
use crate::swarm::bee::Bee;
use crate::swarm::file::file_reference;
use crate::swarm::soc::{self, soc_address};

/// Largest spill or pages file read (a busy address in a large slice).
const MAX_FILE: usize = 256 << 20;

/// Reads `addr1` records of one owner on one chain, checking signatures and identities.
pub struct IndexReader {
    pub bee: Bee,
    pub owner: Address,
    pub chain_id: u64,
}

impl IndexReader {
    /// The payload of the owner's SOC with identifier `id`.
    pub async fn soc(&self, id: B256) -> Result<Vec<u8>> {
        let address = soc_address(&id, &self.owner);
        let raw = self.bee.get_chunk(&address).await?;
        soc::verify(&raw, &address, &self.owner)
    }

    pub async fn origin(&self) -> Result<HeadRecord> {
        HeadRecord::decode(
            &self
                .soc(origin_id(self.chain_id))
                .await
                .context("reading the origin")?,
        )
    }

    pub async fn head(&self, n: u64) -> Result<HeadRecord> {
        HeadRecord::decode(
            &self
                .soc(head_id(self.chain_id, n))
                .await
                .with_context(|| format!("reading head update {n}"))?,
        )
    }

    pub async fn slice(&self, s: SliceId) -> Result<SliceRecord> {
        let rec = SliceRecord::decode(
            &self
                .soc(slice_id(self.chain_id, s.0, s.1))
                .await
                .with_context(|| format!("reading slice ({}, {})", s.0, s.1))?,
        )?;
        ensure!(
            (rec.level, rec.index) == s,
            "slice ({}, {}) holds the record of ({}, {})",
            s.0,
            s.1,
            rec.level,
            rec.index
        );
        Ok(rec)
    }

    /// Bucket `k` of slice `s`: its payload (for `buckets_root`) and its record.
    pub async fn bucket(&self, s: SliceId, k: u32) -> Result<(Vec<u8>, BucketRecord)> {
        let payload = self
            .soc(bucket_id(self.chain_id, s.0, s.1, k))
            .await
            .with_context(|| format!("reading bucket {k} of slice ({}, {})", s.0, s.1))?;
        let rec = BucketRecord::decode(&payload)?;
        ensure!(
            (rec.level, rec.index, rec.bucket) == (s.0, s.1, k),
            "bucket {k} of slice ({}, {}) holds bucket {} of ({}, {})",
            s.0,
            s.1,
            rec.bucket,
            rec.level,
            rec.index
        );
        Ok((payload, rec))
    }

    /// A spill or pages file, checked against its reference.
    pub async fn file(&self, reference: &B256) -> Result<Vec<u8>> {
        let data = self.bee.get_bytes(reference, MAX_FILE).await?;
        ensure!(
            file_reference(&data) == *reference,
            "file {reference} does not hash to its reference"
        );
        Ok(data)
    }
}

/// What `addr-verify` read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub units: u64,
    pub pending: Vec<SliceId>,
    /// Each slice read, with its `buckets_root`.
    pub slices: Vec<(SliceId, B256)>,
    pub buckets: u64,
    pub files: u64,
    pub addresses: u64,
    pub appearances: u64,
}

/// Reads the head announcing `units` (default: the origin's), resolves what a client reads
/// for it, and checks every slice completely: the descriptor's `buckets_root` against the
/// buckets, each spill and pages file against its reference, every entry against the slice's
/// blocks, and the address and appearance counts against the descriptor.
pub async fn verify_published(r: &IndexReader, units: Option<u64>) -> Result<Report> {
    let origin = r.origin().await?;
    let units = units.unwrap_or(origin.units);
    ensure!(
        units >= origin.units,
        "head with {units} units requested, but the index starts at {}",
        origin.units
    );
    let head = r.head(units - origin.units).await?;
    ensure!(
        head.units == units,
        "head update {} announces {} units, expected {units}",
        units - origin.units,
        head.units
    );
    let mut report = Report {
        units,
        pending: head.pending.clone(),
        ..Default::default()
    };
    for s in resolve(units, &head.pending) {
        let rec = r.slice(s).await?;
        let (first, last) = (first_block(s.0, s.1), last_block(s.0, s.1));
        let mut leaves = Vec::with_capacity(rec.bucket_count as usize);
        let (mut addresses, mut appearances) = (0u64, 0u64);
        for k in 0..rec.bucket_count {
            let (payload, bucket) = r.bucket(s, k).await?;
            leaves.push(keccak256(&payload));
            let records: Vec<AddressRecord> = match bucket.body {
                BucketBody::Inline(records) => records,
                BucketBody::Paged {
                    count, pages_ref, ..
                } => {
                    let file = r.file(&pages_ref).await?;
                    report.files += 1;
                    let records = pages(&file)?
                        .into_iter()
                        .map(|p| decode_bucket_page(p, first, last))
                        .collect::<Result<Vec<_>>>()?
                        .concat();
                    ensure!(
                        records.len() as u64 == count as u64,
                        "paged bucket {k} of ({}, {}) states {count} records, holds {}",
                        s.0,
                        s.1,
                        records.len()
                    );
                    records
                }
            };
            for rec in records {
                addresses += 1;
                appearances += match rec.entries {
                    Entries::Inline(e) => e.len() as u64,
                    Entries::Spilled { count, spill_ref } => {
                        let file = r.file(&spill_ref).await?;
                        report.files += 1;
                        let n: usize = pages(&file)?
                            .into_iter()
                            .map(|p| decode_spill_page(p, first, last).map(|e| e.len()))
                            .sum::<Result<usize>>()?;
                        ensure!(
                            n as u64 == count,
                            "spill file {spill_ref} states {count} entries, holds {n}"
                        );
                        count
                    }
                };
            }
        }
        let root = Tree::new(&leaves, depth_for(leaves.len())).root();
        ensure!(
            root == rec.buckets_root,
            "slice ({}, {}): buckets hash to {root}, the descriptor states {}",
            s.0,
            s.1,
            rec.buckets_root
        );
        ensure!(
            addresses == rec.addresses as u64 && appearances == rec.appearances,
            "slice ({}, {}): {addresses} addresses and {appearances} appearances, the \
             descriptor states {} and {}",
            s.0,
            s.1,
            rec.addresses,
            rec.appearances
        );
        report.buckets += rec.bucket_count as u64;
        report.addresses += addresses;
        report.appearances += appearances;
        report.slices.push((s, rec.buckets_root));
    }
    Ok(report)
}
