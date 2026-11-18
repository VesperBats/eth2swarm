//! `addr-lookup`: a client of the published address index (`docs/address-index.md`, "Client
//! lookup"): find the head, resolve the cover, and read one bucket per slice, newest first.

use alloy::primitives::{Address, keccak256};
use alloy::providers::Provider;
use anyhow::{Context, Result, ensure};

use super::fetch::fetch_appearances;
use super::layout::{
    AddressRecord, BucketBody, Entries, Entry, HeadRecord, UNIT_BLOCKS, decode_bucket_page,
    decode_spill_page, first_block, last_block, pages,
};
use super::slice::{bucket_of, fingerprint, resolve, resolve_after};
use super::verify::IndexReader;

/// The newest head the client can find. A client that knows the finalized block `F` aims at
/// update `⌊(F + 1) / 64⌋ − N0`; a miss means the importer is behind, and the client bisects
/// between the origin (update 0, which exists) and that update.
pub async fn latest_head(r: &IndexReader, finalized: u64) -> Result<HeadRecord> {
    let origin = r.origin().await?;
    let aim = ((finalized + 1) / UNIT_BLOCKS).saturating_sub(origin.units);
    if let Ok(head) = r.head(aim).await {
        return Ok(head);
    }
    let (mut lo, mut hi) = (0u64, aim);
    let mut best = origin;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match r.head(mid).await {
            Ok(h) => {
                best = h;
                lo = mid;
            }
            Err(_) => hi = mid,
        }
    }
    Ok(best)
}

/// The appearances of `address` in the index as of `head`, newest first. With `since`, only
/// the slices that end after unit `since` are read (incremental sync), and entries before
/// that unit are dropped. Records are matched by fingerprint, so in rare cases entries of
/// another address with the same 64-bit fingerprint come along (see [`against_chain`]).
pub async fn lookup(
    r: &IndexReader,
    address: Address,
    head: &HeadRecord,
    since: Option<u64>,
) -> Result<Vec<Entry>> {
    let h = keccak256(address.as_slice());
    let fp = fingerprint(&h);
    let slices = match since {
        Some(m) => resolve_after(m, head.units, &head.pending),
        None => resolve(head.units, &head.pending),
    };
    let min_block = since.map_or(0, |m| m * UNIT_BLOCKS);
    let mut out = Vec::new();
    for s in slices.into_iter().rev() {
        let rec = r.slice(s).await?;
        let (first, last) = (first_block(s.0, s.1), last_block(s.0, s.1));
        let k = bucket_of(&h, rec.bucket_count);
        let (_, bucket) = r.bucket(s, k).await?;
        let records: Vec<AddressRecord> = match bucket.body {
            BucketBody::Inline(records) => records,
            BucketBody::Paged { pages_ref, .. } => {
                // A client with little bandwidth binary-searches the pages; this one reads
                // the file.
                let file = r.file(&pages_ref).await?;
                pages(&file)?
                    .into_iter()
                    .map(|p| decode_bucket_page(p, first, last))
                    .collect::<Result<Vec<_>>>()?
                    .concat()
            }
        };
        let mut entries = Vec::new();
        for rec in records.into_iter().filter(|x| x.fp == fp) {
            match rec.entries {
                Entries::Inline(e) => entries.extend(e),
                Entries::Spilled { spill_ref, .. } => {
                    let file = r.file(&spill_ref).await?;
                    for p in pages(&file)? {
                        entries.extend(decode_spill_page(p, first, last)?);
                    }
                }
            }
        }
        entries.retain(|e| e.block >= min_block);
        entries.sort_by_key(|e| std::cmp::Reverse((e.block, e.slot)));
        out.extend(entries);
    }
    Ok(out)
}

/// What [`against_chain`] found.
#[derive(Debug, Default)]
pub struct ChainCheck {
    /// Index entries the chain does not have (or has with other roles).
    pub not_on_chain: Vec<Entry>,
    /// Appearances on the chain the index lacks.
    pub missing: Vec<Entry>,
}

/// Detects every block of `[from_block, units · 64)` directly and compares the appearances of
/// `address` with `found`. Both lists empty means the index is sound and complete for it.
pub async fn against_chain<P>(
    provider: &P,
    address: Address,
    found: &[Entry],
    units: u64,
    from_block: u64,
    traces: bool,
) -> Result<ChainCheck>
where
    P: Provider + Clone + 'static,
{
    ensure!(units >= 1, "no units");
    let mut chain = Vec::new();
    fetch_appearances(
        provider,
        from_block..=units * UNIT_BLOCKS - 1,
        traces,
        16,
        |b| {
            chain.extend(
                b.appearances
                    .into_iter()
                    .filter(|a| a.address == address)
                    .map(|a| Entry {
                        block: a.block,
                        slot: a.slot,
                        roles: a.roles,
                    }),
            );
            Ok(())
        },
    )
    .await
    .context("detecting the range from the chain")?;
    let mut check = ChainCheck::default();
    for e in found {
        if !chain.contains(e) {
            check.not_on_chain.push(*e);
        }
    }
    for e in &chain {
        if !found.contains(e) {
            check.missing.push(*e);
        }
    }
    Ok(check)
}
