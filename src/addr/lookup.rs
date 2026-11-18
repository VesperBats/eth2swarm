//! `addr-lookup`: a client of the published address index (`docs/address-index.md`, "Client
//! lookup"): find the head, resolve the cover, and read one bucket per slice, newest first.

use alloy::primitives::{Address, B256, keccak256};
use alloy::providers::Provider;
use anyhow::{Context, Result, ensure};

use super::fetch::fetch_appearances;
use super::layout::{Entries, Entry, HeadRecord, UNIT_BLOCKS};
use super::slice::{SliceId, bucket_of, fingerprint, resolve, resolve_after};
use super::verify::IndexReader;

/// The newest head the client can find, and whether it is older than the one aimed at. A
/// client that knows the finalized block `F` aims at update `⌊(F + 1) / 64⌋ − N0`. If that
/// update cannot be read, the client bisects between the origin (update 0, which exists) and
/// it. Bee 2.8.3 answers a missing chunk and a failed retrieval alike (500), so an unreadable
/// update can also be a transient failure: the caller warns when the result is older.
pub async fn latest_head(r: &IndexReader, finalized: u64) -> Result<(HeadRecord, bool)> {
    let origin = r.origin().await?;
    let aim = ((finalized + 1) / UNIT_BLOCKS).saturating_sub(origin.units);
    if let Ok(head) = r.head(aim).await {
        return Ok((head, false));
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
    Ok((best, true))
}

/// What a lookup found.
#[derive(Debug, Default)]
pub struct Found {
    /// The entries, newest first.
    pub entries: Vec<Entry>,
    pub slices: usize,
    /// Spill and pages files read for this address.
    pub files: u64,
}

/// Slices read at the same time.
const CONCURRENCY: usize = 8;

/// The entries of the address with hash `h` in slice `s`, with the files read.
async fn in_slice(r: IndexReader, s: SliceId, h: B256) -> Result<(Vec<Entry>, u64)> {
    let rec = r.slice(s).await?;
    let fp = fingerprint(&h);
    let (_, records, mut files) = r.bucket_records(s, bucket_of(&h, rec.bucket_count)).await?;
    let mut entries = Vec::new();
    for rec in records.into_iter().filter(|x| x.fp == fp) {
        match rec.entries {
            Entries::Inline(e) => entries.extend(e),
            Entries::Spilled { count, spill_ref } => {
                entries.extend(r.spill_entries(s, &spill_ref, count).await?);
                files += 1;
            }
        }
    }
    Ok((entries, files))
}

/// The appearances of `address` in the index as of `head`, newest first, reading the slices
/// in parallel. With `since`, only the slices that end after unit `since` are read
/// (incremental sync), and entries before that unit are dropped. Records are matched by
/// fingerprint, so in rare cases entries of another address with the same 64-bit fingerprint
/// come along (see [`against_chain`]).
pub async fn lookup(
    r: &IndexReader,
    address: Address,
    head: &HeadRecord,
    since: Option<u64>,
) -> Result<Found> {
    let h = keccak256(address.as_slice());
    let slices = match since {
        Some(m) => resolve_after(m, head.units, &head.pending),
        None => resolve(head.units, &head.pending),
    };
    let min_block = since.map_or(0, |m| m * UNIT_BLOCKS);
    let mut results = std::collections::BTreeMap::new();
    let mut set = tokio::task::JoinSet::new();
    let mut next = 0;
    while next < slices.len() || !set.is_empty() {
        while set.len() < CONCURRENCY && next < slices.len() {
            let (reader, s, i) = (r.clone(), slices[next], next);
            set.spawn(async move { (i, in_slice(reader, s, h).await) });
            next += 1;
        }
        if let Some(done) = set.join_next().await {
            let (i, res) = done.context("slice read panicked")?;
            results.insert(i, res?);
        }
    }
    let mut found = Found {
        slices: slices.len(),
        ..Default::default()
    };
    // Newest slice first; within a slice, newest entry first.
    for (_, (mut entries, files)) in results.into_iter().rev() {
        entries.retain(|e| e.block >= min_block);
        entries.sort_by_key(|e| std::cmp::Reverse((e.block, e.slot)));
        found.entries.extend(entries);
        found.files += files;
    }
    Ok(found)
}

/// What [`against_chain`] found.
#[derive(Debug, Default)]
pub struct ChainCheck {
    /// Index entries the chain does not have (or has with other roles), duplicates included.
    pub not_on_chain: Vec<Entry>,
    /// Appearances on the chain the index lacks.
    pub missing: Vec<Entry>,
}

/// Detects every block of `[from_block, units · 64)` directly and compares the appearances of
/// `address` with `found`, as multisets: an entry listed twice by the index is a difference.
/// Both lists empty means the index is sound and complete for this address.
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
    Ok(diff(found, chain))
}

/// The multiset difference of index entries and chain appearances, both ways.
fn diff(found: &[Entry], mut chain: Vec<Entry>) -> ChainCheck {
    let mut found = found.to_vec();
    found.sort();
    chain.sort();
    let mut check = ChainCheck::default();
    let (mut i, mut j) = (0, 0);
    while i < found.len() || j < chain.len() {
        match (found.get(i), chain.get(j)) {
            (Some(a), Some(b)) if a == b => {
                i += 1;
                j += 1;
            }
            (Some(a), Some(b)) if a < b => {
                check.not_on_chain.push(*a);
                i += 1;
            }
            (Some(_), Some(b)) => {
                check.missing.push(*b);
                j += 1;
            }
            (Some(a), None) => {
                check.not_on_chain.push(*a);
                i += 1;
            }
            (None, Some(b)) => {
                check.missing.push(*b);
                j += 1;
            }
            (None, None) => unreachable!(),
        }
    }
    check
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(block: u64, slot: u32, roles: u8) -> Entry {
        Entry { block, slot, roles }
    }

    #[test]
    fn diff_is_a_multiset_difference() {
        let chain = vec![e(1, 1, 1), e(2, 1, 2), e(3, 0, 0x40)];
        assert!(
            diff(&[e(3, 0, 0x40), e(1, 1, 1), e(2, 1, 2)], chain.clone())
                .not_on_chain
                .is_empty()
        );
        // A duplicate in the index, a wrong role, and a missing entry.
        let c = diff(&[e(1, 1, 1), e(1, 1, 1), e(2, 1, 3)], chain);
        assert_eq!(c.not_on_chain, vec![e(1, 1, 1), e(2, 1, 3)]);
        assert_eq!(c.missing, vec![e(2, 1, 2), e(3, 0, 0x40)]);
    }
}
