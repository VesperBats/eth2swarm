//! Reading the published address index back from Swarm, and `addr-verify`: a full check of
//! what one head makes a client read.

use alloy::primitives::{Address, B256, keccak256};
use alloy::providers::Provider;
use anyhow::{Context, Result, ensure};

use super::layout::{
    AddressRecord, BucketBody, BucketRecord, Entries, HeadRecord, SliceRecord, bucket_id,
    bucket_pages_file, decode_bucket_page, decode_spill_page, first_block, head_id, last_block,
    origin_id, pages, slice_id, spill_file,
};
use super::slice::{SliceId, resolve};
use crate::accumulator::{Tree, depth_for};
use crate::swarm::bee::Bee;
use crate::swarm::file::file_reference;
use crate::swarm::soc::{self, soc_address};

/// Largest spill or pages file read (a busy address in a large slice).
const MAX_FILE: usize = 256 << 20;

/// Reads `addr1` records of one owner on one chain, checking signatures and identities.
#[derive(Clone)]
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
    /// The head's `last_block_timestamp`, checked against the chain by [`check_chain`].
    pub last_block_timestamp: u64,
    /// Each slice read, with its `buckets_root` and `last_block_hash`.
    pub slices: Vec<(SliceId, B256, B256)>,
    pub buckets: u64,
    pub files: u64,
    pub addresses: u64,
    pub appearances: u64,
}

/// Buckets read at the same time.
const CONCURRENCY: usize = 16;

/// What one bucket holds, checked: its hash for `buckets_root`, and its counts.
struct BucketCheck {
    leaf: B256,
    addresses: u64,
    appearances: u64,
    files: u64,
}

/// Reads bucket `k` of slice `s` with every file it references, and checks them.
async fn check_bucket(r: IndexReader, s: SliceId, k: u32) -> Result<BucketCheck> {
    let (first, last) = (first_block(s.0, s.1), last_block(s.0, s.1));
    let (payload, bucket) = r.bucket(s, k).await?;
    let mut c = BucketCheck {
        leaf: keccak256(&payload),
        addresses: 0,
        appearances: 0,
        files: 0,
    };
    let records: Vec<AddressRecord> = match bucket.body {
        BucketBody::Inline(records) => records,
        BucketBody::Paged {
            count,
            pages: page_count,
            pages_ref,
        } => {
            let file = r.file(&pages_ref).await?;
            c.files += 1;
            let split = pages(&file)?;
            ensure!(
                split.len() as u64 == page_count as u64,
                "paged bucket {k} of ({}, {}) states {page_count} pages, its file has {}",
                s.0,
                s.1,
                split.len()
            );
            let records = split
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
            // A client binary-searches the pages: the order must hold across them, and the
            // file must be the canonical (greedy, padded) encoding of its records.
            ensure!(
                records.windows(2).all(|w| w[0].fp <= w[1].fp),
                "paged bucket {k} of ({}, {}): records not ascending across pages",
                s.0,
                s.1
            );
            ensure!(
                bucket_pages_file(&records, first) == file,
                "paged bucket {k} of ({}, {}): pages file not canonically encoded",
                s.0,
                s.1
            );
            records
        }
    };
    for rec in records {
        c.addresses += 1;
        c.appearances += match rec.entries {
            Entries::Inline(e) => e.len() as u64,
            Entries::Spilled { count, spill_ref } => {
                let file = r.file(&spill_ref).await?;
                c.files += 1;
                let entries = pages(&file)?
                    .into_iter()
                    .map(|p| decode_spill_page(p, first, last))
                    .collect::<Result<Vec<_>>>()?
                    .concat();
                ensure!(
                    entries.len() as u64 == count,
                    "spill file {spill_ref} states {count} entries, holds {}",
                    entries.len()
                );
                ensure!(
                    entries
                        .windows(2)
                        .all(|w| (w[0].block, w[0].slot) < (w[1].block, w[1].slot)),
                    "spill file {spill_ref}: entries not ascending across pages"
                );
                ensure!(
                    spill_file(&entries) == file,
                    "spill file {spill_ref} not canonically encoded"
                );
                count
            }
        };
    }
    Ok(c)
}

/// Reads the head announcing `units` (default: the origin's), resolves what a client reads
/// for it, and checks every slice completely: the descriptor's `buckets_root` against the
/// buckets, each spill and pages file against its reference, every entry against the slice's
/// blocks, the address and appearance counts against the descriptor. Across slices: the same
/// roles, and the head's last block is the newest slice's. [`check_chain`] compares the
/// slices with the chain.
pub async fn verify_published(r: &IndexReader, units: Option<u64>) -> Result<Report> {
    let origin = r.origin().await?;
    let units = units.unwrap_or(origin.units);
    ensure!(
        units >= origin.units,
        "head with {units} units requested, but the index starts at {}",
        origin.units
    );
    let head = r.head(units - origin.units).await?;
    // The origin is a copy of head update 0.
    let head0 = if units == origin.units {
        head.clone()
    } else {
        r.head(0).await?
    };
    ensure!(head0 == origin, "the origin differs from head update 0");
    ensure!(
        head.units == units,
        "head update {} announces {} units, expected {units}",
        units - origin.units,
        head.units
    );
    let mut report = Report {
        units,
        pending: head.pending.clone(),
        last_block_timestamp: head.last_block_timestamp,
        ..Default::default()
    };
    let mut roles = None;
    for s in resolve(units, &head.pending) {
        let rec = r.slice(s).await?;
        ensure!(
            *roles.get_or_insert(rec.roles) == rec.roles,
            "slice ({}, {}) detects roles {:#04x}, another slice {:#04x}",
            s.0,
            s.1,
            rec.roles,
            roles.unwrap_or_default()
        );
        // Results arrive out of order; nothing is sized from the descriptor before the buckets
        // are read.
        let mut checks: std::collections::BTreeMap<u32, BucketCheck> = Default::default();
        let mut set = tokio::task::JoinSet::new();
        let mut next = 0u32;
        while next < rec.bucket_count || !set.is_empty() {
            while set.len() < CONCURRENCY && next < rec.bucket_count {
                let (reader, k) = (r.clone(), next);
                set.spawn(async move { (k, check_bucket(reader, s, k).await) });
                next += 1;
            }
            if let Some(done) = set.join_next().await {
                let (k, c) = done.context("bucket check panicked")?;
                checks.insert(k, c?);
            }
        }
        let checks: Vec<BucketCheck> = checks.into_values().collect();
        let leaves: Vec<B256> = checks.iter().map(|c| c.leaf).collect();
        let root = Tree::new(&leaves, depth_for(leaves.len())).root();
        ensure!(
            root == rec.buckets_root,
            "slice ({}, {}): buckets hash to {root}, the descriptor states {}",
            s.0,
            s.1,
            rec.buckets_root
        );
        let addresses: u64 = checks.iter().map(|c| c.addresses).sum();
        let appearances: u64 = checks.iter().map(|c| c.appearances).sum();
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
        report.files += checks.iter().map(|c| c.files).sum::<u64>();
        report.addresses += addresses;
        report.appearances += appearances;
        report
            .slices
            .push((s, rec.buckets_root, rec.last_block_hash));
    }
    let newest = report.slices.last().map(|x| x.2);
    ensure!(
        newest == Some(head.last_block_hash),
        "the head's last block {} is not the newest slice's {newest:?}",
        head.last_block_hash
    );
    Ok(report)
}

/// Checks every slice's last block against the chain: the index is about this chain, and
/// its slices all lie on it.
pub async fn check_chain(provider: &impl Provider, report: &Report) -> Result<()> {
    for (i, ((level, index), _, hash)) in report.slices.iter().enumerate() {
        let n = last_block(*level, *index);
        let block = provider
            .get_block_by_number(n.into())
            .await
            .with_context(|| format!("querying block {n}"))?
            .with_context(|| format!("block {n} not found"))?;
        ensure!(
            block.header.hash == *hash,
            "slice ({level}, {index}) ends in block {hash}, the chain's block {n} is {}",
            block.header.hash
        );
        // The newest slice ends in the head's last block, whose timestamp clients act on.
        if i + 1 == report.slices.len() {
            ensure!(
                block.header.timestamp == report.last_block_timestamp,
                "the head states timestamp {} for block {n}, the chain {}",
                report.last_block_timestamp,
                block.header.timestamp
            );
        }
    }
    Ok(())
}
