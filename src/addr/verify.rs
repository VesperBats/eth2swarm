//! Reading the published address index back from Swarm, and `addr-verify`: a full check of
//! what one head makes a client read.

use alloy::primitives::{Address, B256, keccak256};
use alloy::providers::Provider;
use anyhow::{Context, Result, ensure};

use super::layout::{
    AddressRecord, BucketBody, BucketRecord, Entries, Entry, HeadRecord, MAX_FILE, PAGE_SIZE,
    SliceRecord, bucket_id, bucket_pages_file, decode_bucket_page, decode_spill_page, entries_len,
    head_id, held_first_block, last_block, origin_id, pages, slice_id, spill_file,
};
use super::slice::{SliceId, resolve_after};
use crate::accumulator::{Tree, depth_for};
use crate::swarm::bee::Bee;
use crate::swarm::file::{CachedChunks, file_reference, file_span, read_leaf, read_leaves};

/// The largest spill file read whole (one `GET /bytes`); a larger one is read page by page
/// through its chunk tree. `ETH2SWARM_ADDR_WHOLE_FILE_BYTES` lowers it, a test hook.
fn whole_file_limit() -> Result<usize> {
    match std::env::var("ETH2SWARM_ADDR_WHOLE_FILE_BYTES") {
        Ok(v) => Ok(v
            .parse::<usize>()
            .with_context(|| format!("ETH2SWARM_ADDR_WHOLE_FILE_BYTES={v}"))?
            .min(MAX_FILE)),
        Err(_) => Ok(MAX_FILE),
    }
}

/// Bytes an entry takes at most in a spill page: a 10-byte block delta, a 5-byte slot, roles.
const ENTRY_MAX: u64 = 16;

/// How a spill file is read: whole, or page by page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpillRead {
    Whole,
    Paged,
}

/// Chunks of a large spill file fetched at a time.
const PAGE_READS: usize = 16;

/// A spill page's bytes before its padding: `count`, `first_block` and its entries.
fn page_content(entries: &[Entry]) -> usize {
    6 + entries_len(entries, entries[0].block)
}
use crate::swarm::soc::{self, soc_address};

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

    /// The descriptor of slice `s` in an index from unit `first_unit` (the head's): it must
    /// start where that index makes it start.
    pub async fn slice(&self, s: SliceId, first_unit: u64) -> Result<SliceRecord> {
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
        let first = held_first_block(s.0, s.1, first_unit);
        ensure!(
            rec.first_block == first,
            "slice ({}, {}) starts at block {}, the index makes it start at {first}",
            s.0,
            s.1,
            rec.first_block
        );
        Ok(rec)
    }

    /// Bucket `k` of the slice `rec` describes: its payload (for `buckets_root`) and its record.
    pub async fn bucket(&self, rec: &SliceRecord, k: u32) -> Result<(Vec<u8>, BucketRecord)> {
        let s = (rec.level, rec.index);
        let payload = self
            .soc(bucket_id(self.chain_id, s.0, s.1, k))
            .await
            .with_context(|| format!("reading bucket {k} of slice ({}, {})", s.0, s.1))?;
        let b = BucketRecord::decode(&payload, rec.first_block)?;
        ensure!(
            (b.level, b.index, b.bucket) == (s.0, s.1, k),
            "bucket {k} of slice ({}, {}) holds bucket {} of ({}, {})",
            s.0,
            s.1,
            b.bucket,
            b.level,
            b.index
        );
        Ok((payload, b))
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

    /// The address records of bucket `k` of the slice `rec` describes, from the bucket or its
    /// pages file,
    /// with the bucket payload (for `buckets_root`) and the number of files read. A pages
    /// file must state the page count the bucket gives, keep the records ascending across
    /// its pages, and be their canonical encoding: what a client's page search relies on.
    pub async fn bucket_records(
        &self,
        rec: &SliceRecord,
        k: u32,
    ) -> Result<(Vec<u8>, Vec<AddressRecord>, u64)> {
        let s = (rec.level, rec.index);
        let (first, last) = (rec.first_block, rec.last_block());
        let (payload, bucket) = self.bucket(rec, k).await?;
        let (records, files) = match bucket.body {
            BucketBody::Inline(records) => (records, 0),
            BucketBody::Paged {
                count,
                pages: page_count,
                pages_ref,
            } => {
                let file = self.file(&pages_ref).await?;
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
                (records, 1)
            }
        };
        Ok((payload, records, files))
    }

    /// Whether the spill file of a record of `count` entries is read whole or page by page. A
    /// count small enough that the file cannot exceed the whole-file limit needs no request;
    /// otherwise its root chunk says the size.
    async fn spill_read(&self, spill_ref: &B256, count: u64) -> Result<SpillRead> {
        let limit = whole_file_limit()? as u64;
        // Pages hold at least 255 entries each, plus their header and padding.
        if count
            .saturating_mul(ENTRY_MAX + 1)
            .saturating_add(PAGE_SIZE as u64)
            <= limit
        {
            return Ok(SpillRead::Whole);
        }
        Ok(if file_span(&self.bee, *spill_ref).await? > limit {
            SpillRead::Paged
        } else {
            SpillRead::Whole
        })
    }

    /// The entries of a spilled address record of the slice `rec` describes, checked: the
    /// stated count, ascending across pages, and the canonical encoding. A file above the
    /// whole-file limit is read page by page (the same checks, page against page).
    pub async fn spill_entries(
        &self,
        rec: &SliceRecord,
        spill_ref: &B256,
        count: u64,
    ) -> Result<Vec<Entry>> {
        match self.spill_read(spill_ref, count).await? {
            SpillRead::Whole => self.spill_whole(rec, spill_ref, count).await,
            SpillRead::Paged => {
                let mut entries = Vec::new();
                self.spill_pages(rec, spill_ref, count, &mut |es| {
                    entries.extend_from_slice(es)
                })
                .await?;
                Ok(entries)
            }
        }
    }

    /// Checks a spilled record's file as [`Self::spill_entries`] does, without keeping its
    /// entries: a file read page by page takes one page of memory, whatever its size.
    pub async fn check_spill(&self, rec: &SliceRecord, spill_ref: &B256, count: u64) -> Result<()> {
        match self.spill_read(spill_ref, count).await? {
            SpillRead::Whole => self.spill_whole(rec, spill_ref, count).await.map(drop),
            SpillRead::Paged => self.spill_pages(rec, spill_ref, count, &mut |_| {}).await,
        }
    }

    /// A spill file read whole, checked.
    async fn spill_whole(
        &self,
        rec: &SliceRecord,
        spill_ref: &B256,
        count: u64,
    ) -> Result<Vec<Entry>> {
        let (first, last) = (rec.first_block, rec.last_block());
        let file = self.file(spill_ref).await?;
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
        Ok(entries)
    }

    /// A spill file read page by page through its chunk tree, each page's entries handed to
    /// `page` in order, checked: every page decodes, the entries ascend across pages, each page
    /// is full (the next one's first entry did not fit), only the last is unpadded, and the
    /// count is the stated one.
    async fn spill_pages(
        &self,
        rec: &SliceRecord,
        spill_ref: &B256,
        count: u64,
        page: &mut (dyn FnMut(&[Entry]) + Send),
    ) -> Result<()> {
        let (first, last) = (rec.first_block, rec.last_block());
        let mut seen = 0u64;
        // The previous page: its content length, its length, its last entry.
        let mut prev: Option<(usize, usize, Entry)> = None;
        read_leaves(&self.bee, *spill_ref, u64::MAX, PAGE_READS, &mut |leaf| {
            let es = decode_spill_page(&leaf, first, last)?;
            if let Some((content, _, e)) = prev {
                let next = es[0];
                ensure!(
                    (e.block, e.slot) < (next.block, next.slot),
                    "spill file {spill_ref}: entries not ascending across pages"
                );
                ensure!(
                    content + entries_len(std::slice::from_ref(&next), e.block) > PAGE_SIZE,
                    "spill file {spill_ref} not canonically encoded: a page ends early"
                );
            }
            seen += es.len() as u64;
            ensure!(
                seen <= count,
                "spill file {spill_ref} holds more than the {count} entries it states"
            );
            prev = Some((page_content(&es), leaf.len(), *es.last().unwrap()));
            page(&es);
            Ok(())
        })
        .await?;
        let (content, len, _) = prev.context("an empty spill file")?;
        ensure!(
            content == len,
            "spill file {spill_ref} not canonically encoded: its last page is padded"
        );
        ensure!(
            seen == count,
            "spill file {spill_ref} states {count} entries, holds {seen}"
        );
        Ok(())
    }

    /// Up to `limit` of the newest entries of a spilled address record, newest first, and
    /// whether the whole file was read and checked ([`Self::spill_entries`]). A file above the
    /// whole-file limit is read from its last page backwards, as a client shows a busy
    /// address: only those pages are read and checked.
    pub async fn spill_newest(
        &self,
        rec: &SliceRecord,
        spill_ref: &B256,
        count: u64,
        limit: usize,
    ) -> Result<(Vec<Entry>, bool)> {
        if self.spill_read(spill_ref, count).await? == SpillRead::Whole {
            let all = self.spill_whole(rec, spill_ref, count).await?;
            return Ok((all.into_iter().rev().take(limit).collect(), true));
        }
        let (first, last) = (rec.first_block, rec.last_block());
        // The root and the intermediate chunks above the last pages are read once.
        let src = CachedChunks::new(self.bee.clone());
        let pages = file_span(&src, *spill_ref)
            .await?
            .div_ceil(PAGE_SIZE as u64);
        let mut out: Vec<Entry> = Vec::new();
        let mut i = pages;
        while i > 0 && out.len() < limit {
            i -= 1;
            let page = read_leaf(&src, *spill_ref, i).await?;
            let es = decode_spill_page(&page, first, last)?;
            if let Some(newer) = out.last() {
                let e = es.last().unwrap();
                ensure!(
                    (e.block, e.slot) < (newer.block, newer.slot),
                    "spill file {spill_ref}: entries not ascending across pages"
                );
            }
            out.extend(es.into_iter().rev());
        }
        out.truncate(limit);
        Ok((out, false))
    }

    /// The head announcing exactly `units` units, given the origin.
    pub async fn head_at(&self, origin: &HeadRecord, units: u64) -> Result<HeadRecord> {
        ensure!(
            units >= origin.units,
            "head with {units} units requested, but the index starts at {}",
            origin.units
        );
        let head = self.head(units - origin.units).await?;
        ensure!(
            head.units == units,
            "head update {} announces {} units, expected {units}",
            units - origin.units,
            head.units
        );
        same_start(&head, origin, units - origin.units)?;
        Ok(head)
    }

    /// Head update `n` if it can be retrieved. A failed retrieval is `Ok(Err(reason))`: Bee
    /// 2.8.3 answers a missing chunk and an unretrievable one alike, so it may also be
    /// transient. A chunk that is retrieved but fails its signature, decoding or the unit
    /// count `N0 + n` is an error.
    pub async fn probe_head(
        &self,
        origin: &HeadRecord,
        n: u64,
    ) -> Result<std::result::Result<HeadRecord, String>> {
        let address = soc_address(&head_id(self.chain_id, n), &self.owner);
        let raw = match self.bee.get_chunk(&address).await {
            Ok(raw) => raw,
            Err(e) => return Ok(Err(format!("{e:#}"))),
        };
        let head = HeadRecord::decode(&soc::verify(&raw, &address, &self.owner)?)
            .with_context(|| format!("head update {n}"))?;
        ensure!(
            head.units == origin.units + n,
            "head update {n} announces {} units, expected {}",
            head.units,
            origin.units + n
        );
        same_start(&head, origin, n)?;
        Ok(Ok(head))
    }
}

/// Every head of an index states the origin's start.
fn same_start(head: &HeadRecord, origin: &HeadRecord, n: u64) -> Result<()> {
    ensure!(
        head.first_unit == origin.first_unit,
        "head update {n} starts at block {}, the origin at {}",
        head.first_block(),
        origin.first_block()
    );
    Ok(())
}

/// What `addr-verify` read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// The first unit the index covers.
    pub first_unit: u64,
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

/// Reads bucket `k` of the slice `rec` describes with every file it references, and checks
/// them.
async fn check_bucket(r: IndexReader, rec: SliceRecord, k: u32) -> Result<BucketCheck> {
    let (payload, records, mut files) = r.bucket_records(&rec, k).await?;
    let mut c = BucketCheck {
        leaf: keccak256(&payload),
        addresses: records.len() as u64,
        appearances: 0,
        files: 0,
    };
    for address_rec in records {
        c.appearances += match address_rec.entries {
            Entries::Inline(e) => e.len() as u64,
            Entries::Spilled { count, spill_ref } => {
                r.check_spill(&rec, &spill_ref, count).await?;
                files += 1;
                count
            }
        };
    }
    c.files = files;
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
    let head = r.head_at(&origin, units).await?;
    // The origin is a copy of head update 0.
    let head0 = if units == origin.units {
        head.clone()
    } else {
        r.head(0).await?
    };
    ensure!(head0 == origin, "the origin differs from head update 0");
    let mut report = Report {
        first_unit: head.first_unit,
        units,
        pending: head.pending.clone(),
        last_block_timestamp: head.last_block_timestamp,
        ..Default::default()
    };
    let mut roles = None;
    // The slices of the cover that hold units from the index's start on.
    for s in resolve_after(head.first_unit, units, &head.pending) {
        let rec = r.slice(s, head.first_unit).await?;
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
                let (reader, rec, k) = (r.clone(), rec.clone(), next);
                set.spawn(async move { (k, check_bucket(reader, rec, k).await) });
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
