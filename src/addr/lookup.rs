//! `addr-lookup`: a client of the published address index (`docs/address-index.md`, "Client
//! lookup"): find the head, resolve the cover, and read one bucket per slice, newest first.

use alloy::primitives::{Address, B256, keccak256};
use alloy::providers::Provider;
use anyhow::{Context, Result, bail, ensure};

use super::fetch::fetch_appearances;
use super::layout::{Entries, Entry, HeadRecord, MAX_UNITS, UNIT_BLOCKS};
use super::slice::{SliceId, bucket_of, fingerprint, resolve_after};
use super::verify::IndexReader;

/// The newest existing update in `0..=aim`, by `exists(n)` (`Ok(Err(reason))` for a miss):
/// first `aim`, then stepping back (`aim − 1`, `aim − 2`, `aim − 4`, …), then bisecting the
/// last gap. The usual case, an importer one update behind, costs two reads whatever the
/// length of the feed. Returns the update and the reason of the last miss, if any.
pub async fn newest_update<F, Fut>(aim: u64, mut exists: F) -> Result<(u64, Option<String>)>
where
    F: FnMut(u64) -> Fut,
    Fut: std::future::Future<Output = Result<std::result::Result<(), String>>>,
{
    let mut last_miss = match exists(aim).await? {
        Ok(()) => return Ok((aim, None)),
        Err(reason) => reason,
    };
    let (mut found, mut missing, mut step) = (None, aim, 1u64);
    while found.is_none() {
        let n = aim.saturating_sub(step);
        match exists(n).await? {
            Ok(()) => found = Some(n),
            Err(reason) if n == 0 => bail!("not even head update 0 can be read: {reason}"),
            Err(reason) => {
                last_miss = reason;
                missing = n;
                step *= 2;
            }
        }
    }
    let mut found = found.expect("found");
    while missing - found > 1 {
        let mid = found + (missing - found) / 2;
        match exists(mid).await? {
            Ok(()) => found = mid,
            Err(reason) => {
                last_miss = reason;
                missing = mid;
            }
        }
    }
    Ok((found, Some(last_miss)))
}

/// The newest head the client can find, aiming at update `⌊(F + 1) / 64⌋ − N0` for the
/// finalized block `F`, with the reason it settled below that aim, if it did.
pub async fn latest_head(r: &IndexReader, finalized: u64) -> Result<(HeadRecord, Option<String>)> {
    let origin = r.origin().await?;
    let aim = ((finalized + 1) / UNIT_BLOCKS).saturating_sub(origin.units);
    // A miss is expected (the importer publishes a unit after the node finalizes it): fewer
    // retries than for data that must be there.
    let probe = IndexReader {
        bee: r.bee.clone().with_retries(2),
        ..r.clone()
    };
    let (n, miss) = newest_update(aim, |n| {
        let (probe, origin) = (probe.clone(), origin.clone());
        async move { Ok(probe.probe_head(&origin, n).await?.map(|_| ())) }
    })
    .await?;
    let head = r.head_at(&origin, origin.units + n).await?;
    Ok((head, miss))
}

/// The newest existing update at or after `known` (one known to exist) and at most `top`, by
/// `probe(n)` (`Ok(Err(reason))` for a miss): stepping up by 1, 1, 2, 4, … from `known` until a
/// probe misses, then bisecting the last gap. Returns the update, what the probe returned for
/// it if it was probed (not when it is `known` itself), the number of probes, and the reason
/// of the last miss. Started from the last update a client found, a client one update behind
/// pays two probes.
pub async fn newest_by_stepping<T, F, Fut>(
    known: u64,
    top: u64,
    mut probe: F,
) -> Result<(u64, Option<T>, u32, Option<String>)>
where
    F: FnMut(u64) -> Fut,
    Fut: std::future::Future<Output = Result<std::result::Result<T, String>>>,
{
    ensure!(
        known <= top,
        "update {known} beyond the last possible one {top}"
    );
    let (mut lo, mut best, mut probes, mut step) = (known, None, 0u32, 1u64);
    if lo == top {
        return Ok((lo, best, probes, None));
    }
    let mut hi = (lo + step).min(top);
    let mut miss = loop {
        probes += 1;
        match probe(hi).await? {
            Ok(v) => {
                (lo, best) = (hi, Some(v));
                if lo == top {
                    return Ok((lo, best, probes, None));
                }
                // Steps 1, 1, 2, 4, …: one update behind settles in two probes.
                hi = lo.saturating_add(step).min(top);
                step = step.saturating_mul(2);
            }
            Err(reason) => break Some(reason),
        }
    };
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        probes += 1;
        match probe(mid).await? {
            Ok(v) => (lo, best) = (mid, Some(v)),
            Err(reason) => {
                miss = Some(reason);
                hi = mid;
            }
        }
    }
    Ok((lo, best, probes, miss))
}

/// The newest head that can be read without knowing the chain's height (no RPC), by
/// [`newest_by_stepping`] from update `known` (the last one this client found, or 0), with
/// the number of probes and the reason of the last miss. A probe is one attempt and one retry.
/// Bee 2.8.3 answers a missing and an unretrievable chunk alike, so a transient failure can
/// settle the search below the newest update: the miss reason says so. With [`latest_head`],
/// a finalized block number aims the search instead.
pub async fn newest_head_by_probing(
    r: &IndexReader,
    origin: &HeadRecord,
    known: u64,
) -> Result<(HeadRecord, u32, Option<String>)> {
    let probe = IndexReader {
        bee: r.bee.clone().with_retries(2),
        ..r.clone()
    };
    let top = MAX_UNITS - origin.units;
    let (n, found, probes, miss) = newest_by_stepping(known.min(top), top, |n| {
        let (probe, origin) = (probe.clone(), origin.clone());
        async move { probe.probe_head(&origin, n).await }
    })
    .await?;
    let head = match found {
        Some(h) => h,
        // Not probed (the known update itself, or update 0): read it, so that update 0 is
        // checked against the origin too.
        None => {
            let h = r.head_at(origin, origin.units + n).await?;
            ensure!(
                n > 0 || h == *origin,
                "head update 0 differs from the origin"
            );
            h
        }
    };
    Ok((head, probes, miss))
}

/// What a lookup as of `head` reads: the slices (oldest first) and the first unit its
/// entries cover, the index's start or the incremental sync's `since`. A `since` beyond the
/// head's units is refused: that head is older than what was read.
pub fn read_plan(head: &HeadRecord, since: Option<u64>) -> Result<(Vec<SliceId>, u64)> {
    if let Some(m) = since {
        ensure!(
            m <= head.units,
            "read up to unit {m}, but this head has only {} units",
            head.units
        );
    }
    let from = since.unwrap_or(0).max(head.first_unit);
    Ok((resolve_after(from, head.units, &head.pending), from))
}

/// What a lookup found.
#[derive(Debug, Default)]
pub struct Found {
    /// The entries, newest first.
    pub entries: Vec<Entry>,
    pub slices: usize,
    /// Spill and pages files read for this address.
    pub files: u64,
    /// The role bits the index detects (the same in every slice read).
    pub roles: u8,
    /// The first block the entries cover: the index's start, or the incremental sync's.
    /// Nothing before it was looked at.
    pub first_block: u64,
}

/// Slices read at the same time.
const CONCURRENCY: usize = 8;

/// The entries of the address with hash `h` in slice `s` of an index from unit `first_unit`,
/// with the files read.
async fn in_slice(
    r: IndexReader,
    s: SliceId,
    first_unit: u64,
    h: B256,
) -> Result<(Vec<Entry>, u64, u8)> {
    let rec = r.slice(s, first_unit).await?;
    let fp = fingerprint(&h);
    let (_, records, mut files) = r
        .bucket_records(&rec, bucket_of(&h, rec.bucket_count))
        .await?;
    let mut entries = Vec::new();
    for address_rec in records.into_iter().filter(|x| x.fp == fp) {
        match address_rec.entries {
            Entries::Inline(e) => entries.extend(e),
            Entries::Spilled { count, spill_ref } => {
                entries.extend(r.spill_entries(&rec, &spill_ref, count).await?);
                files += 1;
            }
        }
    }
    Ok((entries, files, rec.roles))
}

/// The appearances of `address` in the index as of `head`, newest first, reading the slices
/// in parallel. The index covers the units from the head's `first_unit` on; with `since`,
/// only the slices that end after unit `since` are read (incremental sync), and entries
/// before that unit are dropped. [`Found::first_block`] says where the entries start. Records are matched by
/// fingerprint, so in rare cases entries of another address with the same 64-bit fingerprint
/// come along; [`against_chain`] reports them as entries the chain lacks for this address.
pub async fn lookup(
    r: &IndexReader,
    address: Address,
    head: &HeadRecord,
    since: Option<u64>,
) -> Result<Found> {
    let h = keccak256(address.as_slice());
    let (slices, from) = read_plan(head, since)?;
    let min_block = from * UNIT_BLOCKS;
    let mut results = std::collections::BTreeMap::new();
    let mut set = tokio::task::JoinSet::new();
    let mut next = 0;
    let first_unit = head.first_unit;
    while next < slices.len() || !set.is_empty() {
        while set.len() < CONCURRENCY && next < slices.len() {
            let (reader, s, i) = (r.clone(), slices[next], next);
            set.spawn(async move { (i, in_slice(reader, s, first_unit, h).await) });
            next += 1;
        }
        if let Some(done) = set.join_next().await {
            let (i, res) = done.context("slice read panicked")?;
            results.insert(i, res?);
        }
    }
    let mut found = Found {
        slices: slices.len(),
        first_block: min_block,
        ..Default::default()
    };
    // Newest slice first; within a slice, newest entry first.
    let mut roles = None;
    for (_, (mut entries, files, slice_roles)) in results.into_iter().rev() {
        ensure!(
            *roles.get_or_insert(slice_roles) == slice_roles,
            "the slices detect different roles"
        );
        entries.retain(|e| e.block >= min_block);
        entries.sort_by_key(|e| std::cmp::Reverse((e.block, e.slot)));
        found.entries.extend(entries);
        found.files += files;
    }
    found.roles = roles.unwrap_or_default();
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

/// Detects every block of `[found.first_block, units · 64)` directly and compares the
/// appearances of `address` with `found`, as multisets: an entry listed twice by the index is
/// a difference.
/// Both lists empty means the index is sound and complete for this address.
/// The detection follows the index: with traces if its slices detect `TRACE`. The chain must
/// be the index's: the endpoint's chain ID equals `chain_id`, and its block at the head's last
/// block is the head's `last_block_hash`.
pub async fn against_chain<P>(
    provider: &P,
    address: Address,
    found: &Found,
    head: &HeadRecord,
    chain_id: u64,
) -> Result<ChainCheck>
where
    P: Provider + Clone + 'static,
{
    let endpoint = provider.get_chain_id().await.context("querying chain id")?;
    ensure!(
        endpoint == chain_id,
        "the endpoint serves chain {endpoint}, the index is for chain {chain_id}"
    );
    let last = head.last_block();
    let mut last_hash = None;
    let traces = found.roles & super::detect::role::TRACE != 0;
    let mut chain = Vec::new();
    fetch_appearances(provider, found.first_block..=last, traces, 16, |b| {
        if b.number == last {
            last_hash = Some(b.hash);
        }
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
    })
    .await
    .context("detecting the range from the chain")?;
    ensure!(
        last_hash == Some(head.last_block_hash),
        "the endpoint's block {last} is {last_hash:?}, the index ends in {}: another chain",
        head.last_block_hash
    );
    Ok(diff(&found.entries, chain))
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
        let same = diff(&[e(3, 0, 0x40), e(1, 1, 1), e(2, 1, 2)], chain.clone());
        assert!(same.not_on_chain.is_empty() && same.missing.is_empty());
        // A duplicate in the index, a wrong role, and a missing entry.
        let c = diff(&[e(1, 1, 1), e(1, 1, 1), e(2, 1, 3)], chain);
        assert_eq!(c.not_on_chain, vec![e(1, 1, 1), e(2, 1, 3)]);
        assert_eq!(c.missing, vec![e(2, 1, 2), e(3, 0, 0x40)]);
    }

    /// Runs `newest_update` against updates `0..=newest` existing, counting the reads.
    fn search(aim: u64, newest: u64) -> (u64, usize) {
        let reads = std::cell::Cell::new(0);
        let r = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(newest_update(aim, |n| {
                reads.set(reads.get() + 1);
                async move {
                    Ok(if n <= newest {
                        Ok(())
                    } else {
                        Err(format!("{n} missing"))
                    })
                }
            }))
            .unwrap();
        assert_eq!(r.1.is_some(), newest < aim);
        (r.0, reads.get())
    }

    /// Runs `newest_by_stepping` from `known` against updates `0..=newest` existing.
    fn step_search(known: u64, newest: u64, top: u64) -> (u64, u32) {
        let r = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(newest_by_stepping(known, top, |n| async move {
                Ok(if n <= newest {
                    Ok(n)
                } else {
                    Err(format!("{n} missing"))
                })
            }))
            .unwrap();
        // The update returned is the one probed for it, unless nothing above `known` was
        // found; the last miss is given unless the search reached `top`.
        assert_eq!(r.1, (r.0 > known).then_some(r.0));
        assert_eq!(r.3.is_some(), r.0 < top);
        (r.0, r.2)
    }

    #[test]
    fn read_plan_follows_the_start_and_refuses_a_stale_head() {
        let head = HeadRecord {
            first_unit: 9,
            units: 13,
            last_block_hash: B256::ZERO,
            last_block_timestamp: 0,
            pending: vec![],
        };
        assert_eq!(read_plan(&head, None).unwrap(), (vec![(2, 2), (0, 12)], 9));
        assert_eq!(read_plan(&head, Some(12)).unwrap(), (vec![(0, 12)], 12));
        assert_eq!(read_plan(&head, Some(3)).unwrap().1, 9);
        assert!(read_plan(&head, Some(14)).is_err());
    }

    #[test]
    fn newest_by_stepping_finds_the_newest() {
        let top = 1 << 20;
        for newest in [0u64, 1, 2, 7, 8, 9, 1000, top - 1, top] {
            assert_eq!(step_search(0, newest, top).0, newest, "newest {newest}");
        }
        // From the last update found, one behind costs two probes, up to date one.
        assert_eq!(step_search(999, 1000, top), (1000, 2));
        assert_eq!(step_search(1000, 1000, top), (1000, 1));
        // Far behind: still logarithmic.
        let (n, probes) = step_search(0, 123_456, top);
        assert_eq!(n, 123_456);
        assert!(probes <= 2 * 20 + 2, "{probes} probes");
        // Already at the top: no probe.
        assert_eq!(step_search(top, top, top), (top, 0));
    }

    #[test]
    fn newest_update_steps_back_then_bisects() {
        // Up to date: one read.
        assert_eq!(search(1000, 1000), (1000, 1));
        // One update behind, the usual case: two reads, however long the feed.
        assert_eq!(search(1_000_000, 999_999), (999_999, 2));
        // Far behind: still found, in a logarithmic number of reads.
        let (n, reads) = search(1_000_000, 123_456);
        assert_eq!(n, 123_456);
        assert!(reads <= 2 * 20 + 2, "{reads} reads");
        // Only the origin.
        assert_eq!(search(50, 0).0, 0);
    }
}
