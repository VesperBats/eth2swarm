//! Slices: which slices cover the chain (`docs/address-index.md`, "Slices and covers"), and
//! building one slice from its appearances ("Record formats").

// The importer and client that use all of this follow in parts 4 and 5 of #8.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::ops::Range;

use alloy::primitives::{Address, B256, keccak256};
use anyhow::{Result, ensure};

use super::detect::Appearance;
use super::layout::{
    AddressRecord, BUCKET_TARGET, BucketBody, BucketRecord, Entries, Entry, L_MAX, PAGE_SIZE,
    SPILL_INLINE_MAX, SliceRecord, bucket_pages_file, entries_len, first_block, last_block,
    spill_file,
};
use crate::accumulator::{Tree, depth_for};
use crate::swarm::file::file_reference;

/// A slice `(level, index)`: units `[index·2^level, (index+1)·2^level)`.
pub type SliceId = (u8, u32);

/// Units covered by a slice.
pub fn units(slice: SliceId) -> Range<u64> {
    let (level, index) = slice;
    (index as u64) << level..(index as u64 + 1) << level
}

/// The cover of `[0, n)` units, oldest first.
pub fn cover(n: u64) -> Vec<SliceId> {
    cover_with(n, L_MAX)
}

fn cover_with(n: u64, l_max: u8) -> Vec<SliceId> {
    let mut out: Vec<SliceId> = (0..n >> l_max).map(|j| (l_max, j as u32)).collect();
    let mut start = (n >> l_max) << l_max;
    for level in (0..l_max).rev() {
        if n & (1 << level) != 0 {
            out.push((level, (start >> level) as u32));
            start += 1 << level;
        }
    }
    out
}

/// The one slice the importer writes when unit `n − 1` completes (`n ≥ 1`).
pub fn new_slice(n: u64) -> SliceId {
    new_slice_with(n, L_MAX)
}

fn new_slice_with(n: u64, l_max: u8) -> SliceId {
    assert!(n >= 1, "no slice before the first unit");
    let level = (n.trailing_zeros() as u8).min(l_max);
    (level, ((n >> level) - 1) as u32)
}

/// The slices a deferred merge `(level, index)` replaces while it is pending: the cover of its
/// units but the last, then the last unit's level-0 slice.
pub fn expansion(slice: SliceId) -> Vec<SliceId> {
    let level = slice.0;
    if level == 0 {
        return vec![slice];
    }
    let r = units(slice);
    let mut out = Vec::with_capacity(level as usize + 1);
    // The cover of all units but the last (2^(level−1) + … + 1 = 2^level − 1 units) …
    let mut start = r.start;
    for l in (0..level).rev() {
        out.push((l, (start >> l) as u32));
        start += 1 << l;
    }
    debug_assert_eq!(start, r.end - 1);
    // … then the last unit's level-0 slice.
    out.push((0, start as u32));
    out
}

/// The slices of the cover of `[0, n)` that end after unit `m`: what a client that has read
/// everything up to unit `m` still has to read.
pub fn slices_after(m: u64, n: u64) -> Vec<SliceId> {
    cover(n).into_iter().filter(|s| units(*s).end > m).collect()
}

/// A slice built in memory: its descriptor, its bucket payloads (index = bucket number) and
/// the spill and pages files they reference.
#[derive(Debug, Clone)]
pub struct BuiltSlice {
    pub record: SliceRecord,
    pub buckets: Vec<Vec<u8>>,
    pub files: Vec<Vec<u8>>,
}

/// keccak256 of the address bytes: bucket from `h[0..4]`, fingerprint `h[4..12]`.
fn address_hash(a: &Address) -> B256 {
    keccak256(a.as_slice())
}

/// Bucket of a hash among `buckets` (multiply-shift).
pub fn bucket_of(h: &B256, buckets: u32) -> u32 {
    let prefix = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
    ((prefix * buckets as u64) >> 32) as u32
}

pub fn fingerprint(h: &B256) -> [u8; 8] {
    h[4..12].try_into().unwrap()
}

/// Builds slice `slice` from the appearances of its blocks. `last_block_hash` is the hash of
/// its last block; `roles` the role bits the importer detects.
pub fn build_slice(
    slice: SliceId,
    appearances: &[Appearance],
    last_block_hash: B256,
    roles: u8,
) -> Result<BuiltSlice> {
    build(slice, appearances, last_block_hash, roles, None)
}

/// [`build_slice`], with the bucket count overridable for tests (`B` is otherwise
/// `max(1, ⌈record bytes / T⌉)`).
fn build(
    slice: SliceId,
    appearances: &[Appearance],
    last_block_hash: B256,
    roles: u8,
    force_buckets: Option<u32>,
) -> Result<BuiltSlice> {
    let (level, index) = slice;
    ensure!(level <= L_MAX, "slice level {level} above {L_MAX}");
    let (first, last) = (first_block(level, index), last_block(level, index));

    // Entries per address, merged per (block, slot).
    let mut by_address: BTreeMap<Address, BTreeMap<(u64, u32), u8>> = BTreeMap::new();
    for a in appearances {
        ensure!(
            (first..=last).contains(&a.block),
            "appearance in block {} outside slice ({level}, {index}), blocks {first}..={last}",
            a.block
        );
        *by_address
            .entry(a.address)
            .or_default()
            .entry((a.block, a.slot))
            .or_default() |= a.roles;
    }

    let mut files = Vec::new();
    let mut appearance_count = 0u64;
    // (hash, address, record, encoded length)
    let mut records: Vec<(B256, Address, AddressRecord, usize)> = by_address
        .into_iter()
        .map(|(address, entries)| {
            let entries: Vec<Entry> = entries
                .into_iter()
                .map(|((block, slot), roles)| Entry { block, slot, roles })
                .collect();
            appearance_count += entries.len() as u64;
            let h = address_hash(&address);
            let entries = if entries_len(&entries, first) > SPILL_INLINE_MAX {
                let file = spill_file(&entries);
                let spill_ref = file_reference(&file);
                files.push(file);
                Entries::Spilled {
                    count: entries.len() as u64,
                    spill_ref,
                }
            } else {
                Entries::Inline(entries)
            };
            let rec = AddressRecord {
                fp: fingerprint(&h),
                entries,
            };
            let len = rec.encoded_len(first);
            (h, address, rec, len)
        })
        .collect();

    let total: usize = records.iter().map(|r| r.3).sum();
    let bucket_count = match force_buckets {
        Some(b) => b,
        None => u32::try_from(total.div_ceil(BUCKET_TARGET).max(1))?,
    };

    // Records per bucket, ordered by (fingerprint, address).
    records.sort_by(|a, b| {
        (bucket_of(&a.0, bucket_count), fingerprint(&a.0), a.1).cmp(&(
            bucket_of(&b.0, bucket_count),
            fingerprint(&b.0),
            b.1,
        ))
    });
    let mut grouped: Vec<Vec<AddressRecord>> = vec![Vec::new(); bucket_count as usize];
    for (h, _, rec, _) in records.iter() {
        grouped[bucket_of(h, bucket_count) as usize].push(rec.clone());
    }
    let address_count = u32::try_from(records.len())?;
    drop(records);

    let mut buckets = Vec::with_capacity(grouped.len());
    for (k, recs) in grouped.into_iter().enumerate() {
        let inline = BucketRecord {
            level,
            index,
            bucket: k as u32,
            body: BucketBody::Inline(recs),
        };
        let mut payload = inline.encode();
        if payload.len() > PAGE_SIZE {
            let BucketBody::Inline(recs) = inline.body else {
                unreachable!()
            };
            let file = bucket_pages_file(&recs, first);
            let paged = BucketRecord {
                level,
                index,
                bucket: k as u32,
                body: BucketBody::Paged {
                    count: recs.len() as u32,
                    pages: file.len().div_ceil(PAGE_SIZE) as u32,
                    pages_ref: file_reference(&file),
                },
            };
            files.push(file);
            payload = paged.encode();
        }
        buckets.push(payload);
    }

    let leaves: Vec<B256> = buckets.iter().map(keccak256).collect();
    let buckets_root = Tree::new(&leaves, depth_for(leaves.len())).root();

    Ok(BuiltSlice {
        record: SliceRecord {
            level,
            index,
            last_block_hash,
            bucket_count,
            addresses: address_count,
            appearances: appearance_count,
            buckets_root,
            roles,
        },
        buckets,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::layout::{
        UNIT_BLOCKS, decode_bucket_page, decode_spill_page, pages as split_pages,
    };
    use std::collections::BTreeSet;

    #[test]
    fn cover_examples() {
        assert_eq!(cover_with(13, 16), vec![(3, 0), (2, 2), (0, 12)]);
        assert_eq!(new_slice(13), (0, 12));
        assert_eq!(new_slice(16), (4, 0));
        // Capped at the top level: unit 2^20 completes a top-level slice, not a level-20 one.
        assert_eq!(new_slice(1 << 20), (L_MAX, 15));
        assert!(cover(0).is_empty());
        // Mainnet at block 26,428,827: 6 top-level slices and 7 lower ones.
        let n = 26_428_827 / UNIT_BLOCKS;
        assert_eq!(n, 412_950);
        let c = cover(n);
        assert_eq!(c.iter().filter(|s| s.0 == L_MAX).count(), 6);
        assert_eq!(c.len(), 13);
    }

    /// The invariants the doc relies on, for every `n` over three top-level slices of a small
    /// `l_max` (so the cap is exercised).
    #[test]
    fn cover_invariants() {
        let l_max = 5;
        for n in 1..(3u64 << l_max) + 5 {
            let c = cover_with(n, l_max);
            // A partition of [0, n), oldest first.
            let mut next = 0;
            for s in &c {
                assert_eq!(units(*s).start, next, "n={n}: {c:?}");
                next = units(*s).end;
            }
            assert_eq!(next, n);
            // Below the cap every index is even.
            assert!(
                c.iter().all(|s| s.0 == l_max || s.1 % 2 == 0),
                "n={n}: {c:?}"
            );
            // Exactly one new slice per unit.
            let prev: BTreeSet<_> = cover_with(n - 1, l_max).into_iter().collect();
            let now: BTreeSet<_> = c.iter().copied().collect();
            let added: Vec<_> = now.difference(&prev).copied().collect();
            assert_eq!(added, vec![new_slice_with(n, l_max)], "n={n}");
            // The slices it replaces are exactly the previous cover's slices in its range.
            let new = new_slice_with(n, l_max);
            let replaced: Vec<_> = prev.difference(&now).copied().collect();
            assert!(replaced.iter().all(|s| {
                let (u, r) = (units(*s), units(new));
                r.start <= u.start && u.end <= r.end
            }));
        }
    }

    #[test]
    fn expansion_uses_existing_slices() {
        // A deferred (3, 2) = units 16..24: (2, 4), (1, 10), (0, 22), then (0, 23).
        assert_eq!(expansion((3, 2)), vec![(2, 4), (1, 10), (0, 22), (0, 23)]);
        for slice in [(1u8, 4u32), (4, 2), (16, 6)] {
            let e = expansion(slice);
            assert_eq!(e.len(), slice.0 as usize + 1);
            let n = units(slice).end;
            // All but the last were in the cover just before the merge's unit completed.
            let before: BTreeSet<_> = cover(n - 1).into_iter().collect();
            assert!(
                e[..e.len() - 1].iter().all(|s| before.contains(s)),
                "{slice:?}"
            );
            assert_eq!(*e.last().unwrap(), (0, (n - 1) as u32));
            assert_eq!(units(e[0]).start, units(slice).start);
        }
    }

    #[test]
    fn incremental() {
        // Up to unit 12 read; the cover of 13 has only (0, 12) after it.
        assert_eq!(slices_after(12, 13), vec![(0, 12)]);
        // Read up to unit 13; at 16 the cover is one slice, re-read whole.
        assert_eq!(slices_after(13, 16), vec![(4, 0)]);
    }

    fn app(block: u64, slot: u32, byte: u8, roles: u8) -> Appearance {
        Appearance {
            block,
            slot,
            address: Address::repeat_byte(byte),
            roles,
        }
    }

    #[test]
    fn build_small_slice() {
        let slice = (0, 3);
        let base = first_block(0, 3);
        let apps = vec![
            app(base, 1, 0x11, 1),
            app(base, 1, 0x11, 4), // merged into the first
            app(base + 2, 0, 0x22, 0x40),
            app(base + 63, 7, 0x11, 2),
        ];
        let built = build_slice(slice, &apps, B256::repeat_byte(9), 0xdf).unwrap();
        assert_eq!(built.record.bucket_count, 1);
        assert_eq!(built.record.addresses, 2);
        assert_eq!(built.record.appearances, 3);
        assert!(built.files.is_empty());
        let b = BucketRecord::decode(&built.buckets[0]).unwrap();
        let BucketBody::Inline(recs) = b.body else {
            panic!("inline expected")
        };
        let h = address_hash(&Address::repeat_byte(0x11));
        let r = recs.iter().find(|r| r.fp == fingerprint(&h)).unwrap();
        assert_eq!(
            r.entries,
            Entries::Inline(vec![
                Entry {
                    block: base,
                    slot: 1,
                    roles: 5
                },
                Entry {
                    block: base + 63,
                    slot: 7,
                    roles: 2
                },
            ])
        );
        assert_eq!(built.record.buckets_root, keccak256(&built.buckets[0]));
        // Outside the slice.
        assert!(build_slice(slice, &[app(base + 64, 1, 1, 1)], B256::ZERO, 0).is_err());
        // An empty slice still has one bucket.
        let empty = build_slice((0, 0), &[], B256::ZERO, 0).unwrap();
        assert_eq!(empty.record.bucket_count, 1);
        assert_eq!(empty.buckets.len(), 1);
    }

    #[test]
    fn build_spills_and_pages() {
        let slice = (4, 0); // blocks 0..1024
        let mut apps = Vec::new();
        // One busy address: 400 entries, spilled.
        for b in 0..400 {
            apps.push(app(b, 1, 0xaa, 1));
        }
        // 340 addresses sharing nothing in particular; one entry each.
        for i in 0..2000u32 {
            apps.push(Appearance {
                block: (i % 1024) as u64,
                slot: 2,
                address: Address::left_padding_from(&(i + 1_000_000).to_be_bytes()),
                roles: 0x10,
            });
        }
        let built = build_slice(slice, &apps, B256::ZERO, 0x10).unwrap();
        assert_eq!(built.record.addresses, 2001);
        // Every bucket is at most one chunk.
        assert!(built.buckets.iter().all(|b| b.len() <= PAGE_SIZE));
        // The spilled address: its spill file decodes to its 400 entries.
        let busy = address_hash(&Address::repeat_byte(0xaa));
        let k = bucket_of(&busy, built.record.bucket_count) as usize;
        let BucketBody::Inline(recs) = BucketRecord::decode(&built.buckets[k]).unwrap().body else {
            panic!("inline expected")
        };
        let rec = recs.iter().find(|r| r.fp == fingerprint(&busy)).unwrap();
        let Entries::Spilled { count, spill_ref } = rec.entries else {
            panic!("spilled expected")
        };
        assert_eq!(count, 400);
        let file = built
            .files
            .iter()
            .find(|f| file_reference(f) == spill_ref)
            .unwrap();
        let entries: Vec<Entry> = split_pages(file)
            .unwrap()
            .iter()
            .flat_map(|p| decode_spill_page(p).unwrap())
            .collect();
        assert_eq!(entries.len(), 400);
        assert_eq!(entries[399].block, 399);

        // Deterministic: building again gives identical bytes.
        let again = build_slice(slice, &apps, B256::ZERO, 0x10).unwrap();
        assert_eq!(again.buckets, built.buckets);
        assert_eq!(again.files, built.files);
        assert_eq!(again.record, built.record);
    }

    /// Addresses whose hashes share `h[0..4]` cannot be separated by any bucket count; the
    /// bucket is paged instead of breaking the slice. Grinding real collisions is too slow
    /// for a test, so all addresses are forced into one bucket.
    #[test]
    fn overfull_bucket_is_paged() {
        let slice = (0, 0);
        let apps: Vec<Appearance> = (0..400u32)
            .map(|i| Appearance {
                block: 5,
                slot: 1,
                address: Address::left_padding_from(&(i + 70_000).to_be_bytes()),
                roles: 0x10,
            })
            .collect();
        let built = build(slice, &apps, B256::ZERO, 0x10, Some(1)).unwrap();
        assert_eq!(built.buckets.len(), 1);
        assert!(built.buckets[0].len() <= PAGE_SIZE);
        let b = BucketRecord::decode(&built.buckets[0]).unwrap();
        let BucketBody::Paged {
            count,
            pages,
            pages_ref,
        } = b.body
        else {
            panic!("paged expected")
        };
        assert_eq!(count, 400);
        let file = built
            .files
            .iter()
            .find(|f| file_reference(f) == pages_ref)
            .unwrap();
        let pages_list = split_pages(file).unwrap();
        assert_eq!(pages_list.len(), pages as usize);
        let decoded: Vec<AddressRecord> = pages_list
            .iter()
            .flat_map(|p| decode_bucket_page(p, first_block(0, 0)).unwrap())
            .collect();
        assert_eq!(decoded.len(), 400);
        // Ordered by fingerprint.
        assert!(decoded.windows(2).all(|w| w[0].fp <= w[1].fp));
        // The bucket hash in the root is that of the paged bucket record.
        assert_eq!(built.record.buckets_root, keccak256(&built.buckets[0]));
    }

    #[test]
    fn bucket_assignment_spreads_evenly() {
        let b = 37;
        let mut counts = vec![0u32; b as usize];
        for i in 0..37_000u32 {
            let h = address_hash(&Address::left_padding_from(&i.to_be_bytes()));
            counts[bucket_of(&h, b) as usize] += 1;
        }
        assert!(counts.iter().all(|c| (800..1200).contains(c)), "{counts:?}");
        assert_eq!(bucket_of(&B256::repeat_byte(0xff), 1), 0);
        assert_eq!(bucket_of(&B256::repeat_byte(0xff), b), b - 1);
    }
}
