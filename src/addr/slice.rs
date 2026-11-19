//! Slices: which slices cover the chain (`docs/address-index.md`, "Slices and covers"), and
//! building one slice from its appearances ("Record formats").

use std::ops::Range;

use alloy::primitives::B256;
use anyhow::Result;

use super::build::{Output, SliceBuilder};
use super::detect::Appearance;
use super::layout::{L_MAX, SliceRecord};

/// A slice `(level, index)`: units `[index·2^level, (index+1)·2^level)`.
pub type SliceId = (u8, u32);

/// Units covered by a slice.
pub fn units(slice: SliceId) -> Range<u64> {
    let (level, index) = slice;
    (index as u64) << level..(index as u64 + 1) << level
}

/// Units slice `slice` holds in an index that starts at unit `first_unit`: its own units from
/// the index's start on. Empty for a slice that ends at or before the start: it does not exist
/// in that index.
pub fn held(slice: SliceId, first_unit: u64) -> Range<u64> {
    let r = units(slice);
    r.start.max(first_unit).min(r.end)..r.end
}

/// The size of what `slice` holds in an index from unit `first_unit`, as a level: the
/// smallest `L` with `2^L` units at least [`held`]'s. The slice's own level from genesis, less
/// for the slice a later start falls into. What building it costs grows with this.
pub fn held_level(slice: SliceId, first_unit: u64) -> u8 {
    held(slice, first_unit)
        .count()
        .next_power_of_two()
        .trailing_zeros() as u8
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

/// What a client reads for a head with `n` units and the deferred merges `pending`: the
/// cover of `[0, n)`, with every pending slice replaced by its expansion, recursively (an
/// expansion can contain a merge that is still pending itself). Oldest first.
pub fn resolve(n: u64, pending: &[SliceId]) -> Vec<SliceId> {
    fn push(out: &mut Vec<SliceId>, s: SliceId, pending: &[SliceId]) {
        if s.0 > 0 && pending.contains(&s) {
            for e in expansion(s) {
                push(out, e, pending);
            }
        } else {
            out.push(s);
        }
    }
    let mut out = Vec::new();
    for s in cover(n) {
        push(&mut out, s, pending);
    }
    out
}

/// What a client that has read everything up to unit `m` still has to read for a head with
/// `n` units and the deferred merges `pending`: the slices of [`resolve`] that end after unit
/// `m`. A slice that starts before `m` is read whole; its entries before `m` are not new.
///
/// For an index that starts at unit `S`, `resolve_after(S, …)` is what a client reads: the
/// slices of the cover that end at or before `S` do not exist.
pub fn resolve_after(m: u64, n: u64, pending: &[SliceId]) -> Vec<SliceId> {
    resolve(n, pending)
        .into_iter()
        .filter(|s| units(*s).end > m)
        .collect()
}

/// A slice built in memory: its descriptor, its bucket payloads (index = bucket number) and
/// the spill and pages files they reference, each with its Swarm reference. Files are
/// content-addressed, so a file two addresses share (identical entries) is listed once.
#[derive(Debug, Clone)]
pub struct BuiltSlice {
    pub record: SliceRecord,
    pub buckets: Vec<Vec<u8>>,
    pub files: Vec<(B256, Vec<u8>)>,
    /// Addresses whose entries went to a spill file.
    pub spilled: u32,
    /// Buckets stored as pages files.
    pub paged: u32,
}

/// Bucket of a hash among `buckets` (multiply-shift).
pub fn bucket_of(h: &B256, buckets: u32) -> u32 {
    bucket_of_prefix(u32::from_be_bytes(h[..4].try_into().unwrap()), buckets)
}

/// Bucket of an address whose hash starts with `prefix` (`h[0..4]` big-endian), among
/// `buckets`: what the streaming builder computes from its sort items.
pub fn bucket_of_prefix(prefix: u32, buckets: u32) -> u32 {
    ((prefix as u64 * buckets as u64) >> 32) as u32
}

pub fn fingerprint(h: &B256) -> [u8; 8] {
    h[4..12].try_into().unwrap()
}

/// Builds slice `slice` of an index that starts at unit `first_unit` from the appearances of
/// the blocks it holds ([`held`]). `last_block_hash` is the hash of its last block; `roles`
/// the role bits the importer detects.
pub fn build_slice(
    slice: SliceId,
    first_unit: u64,
    appearances: &[Appearance],
    last_block_hash: B256,
    roles: u8,
) -> Result<BuiltSlice> {
    build(slice, first_unit, appearances, last_block_hash, roles, None)
}

/// [`build_slice`], with the bucket count overridable for tests (`B` is otherwise
/// `max(1, ⌈record bytes / T⌉)`). Builds in memory (one sort buffer for every appearance)
/// and collects the buckets and files; the importer streams them instead
/// (`build::SliceBuilder`).
fn build(
    slice: SliceId,
    first_unit: u64,
    appearances: &[Appearance],
    last_block_hash: B256,
    roles: u8,
    force_buckets: Option<u32>,
) -> Result<BuiltSlice> {
    let mut b = SliceBuilder::new(
        slice,
        first_unit,
        roles,
        &std::env::temp_dir(),
        (appearances.len() + 1) * 64,
    )?
    // Built in memory: every spill file is handed out whole, at any size.
    .with_stream_file(usize::MAX);
    for a in appearances {
        b.push(a)?;
    }
    let (mut buckets, mut files) = (Vec::new(), Vec::new());
    let built = b.finish(last_block_hash, force_buckets, &mut |out| {
        match out {
            Output::File(r, f) => files.push((r, f)),
            Output::Chunk(..) => anyhow::bail!("an in-memory build streamed a spill file"),
            Output::Bucket(k, p) => {
                anyhow::ensure!(k as usize == buckets.len(), "bucket {k} out of order");
                buckets.push(p)
            }
        }
        Ok(())
    })?;
    Ok(BuiltSlice {
        record: built.record,
        buckets,
        files,
        spilled: built.spilled,
        paged: built.paged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accumulator::{Tree, depth_for};
    use crate::addr::layout::{
        AddressRecord, BUCKET_TARGET, BucketBody, BucketRecord, Entries, Entry, PAGE_SIZE,
        SPILL_INLINE_MAX, bucket_pages_file, entries_len, held_first_block, last_block, spill_file,
    };
    use crate::swarm::file::file_reference;
    use alloy::primitives::{Address, keccak256};
    use anyhow::ensure;
    use std::collections::{BTreeMap, HashSet};

    /// keccak256 of the address bytes: bucket from `h[0..4]`, fingerprint `h[4..12]`.
    fn address_hash(a: &Address) -> B256 {
        keccak256(a.as_slice())
    }

    /// Adds `file` under `reference` unless a file with that reference is already there.
    fn add_file(
        files: &mut Vec<(B256, Vec<u8>)>,
        seen: &mut HashSet<B256>,
        reference: B256,
        file: Vec<u8>,
    ) {
        if seen.insert(reference) {
            files.push((reference, file));
        }
    }

    /// The in-memory builder the streaming one replaced, kept as the reference it must match.
    fn reference_build(
        slice: SliceId,
        first_unit: u64,
        appearances: &[Appearance],
        last_block_hash: B256,
        roles: u8,
        force_buckets: Option<u32>,
    ) -> Result<BuiltSlice> {
        let (level, index) = slice;
        ensure!(level <= L_MAX, "slice level {level} above {L_MAX}");
        ensure!(
            !held(slice, first_unit).is_empty(),
            "slice ({level}, {index}) ends before the index starts at unit {first_unit}"
        );
        let (first, last) = (
            held_first_block(level, index, first_unit),
            last_block(level, index),
        );
        ensure!(
            last < 1 << 32,
            "slice ({level}, {index}) ends at block {last}, beyond the format's u32 block numbers"
        );

        // Entries per address, merged per (block, slot).
        let mut by_address: BTreeMap<Address, BTreeMap<(u64, u32), u8>> = BTreeMap::new();
        for a in appearances {
            ensure!(
                a.roles != 0 && a.roles & !roles == 0,
                "appearance with roles {:#04x} outside the slice's {roles:#04x}",
                a.roles
            );
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
        let mut seen = HashSet::new();
        let mut spilled = 0u32;
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
                    add_file(&mut files, &mut seen, spill_ref, file);
                    spilled += 1;
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
        let address_count = u32::try_from(records.len())?;
        let mut grouped: Vec<Vec<AddressRecord>> = vec![Vec::new(); bucket_count as usize];
        for (h, _, rec, _) in records {
            grouped[bucket_of(&h, bucket_count) as usize].push(rec);
        }

        let mut buckets = Vec::with_capacity(grouped.len());
        let mut paged = 0u32;
        for (k, recs) in grouped.into_iter().enumerate() {
            let inline = BucketRecord {
                level,
                index,
                bucket: k as u32,
                base: first,
                body: BucketBody::Inline(recs),
            };
            let mut payload = inline.encode();
            if payload.len() > PAGE_SIZE {
                let BucketBody::Inline(recs) = inline.body else {
                    unreachable!()
                };
                let file = bucket_pages_file(&recs, first);
                let pages_ref = file_reference(&file);
                let paged_bucket = BucketRecord {
                    level,
                    index,
                    bucket: k as u32,
                    base: first,
                    body: BucketBody::Paged {
                        count: recs.len() as u32,
                        pages: file.len().div_ceil(PAGE_SIZE) as u32,
                        pages_ref,
                    },
                };
                add_file(&mut files, &mut seen, pages_ref, file);
                payload = paged_bucket.encode();
                paged += 1;
            }
            buckets.push(payload);
        }

        let leaves: Vec<B256> = buckets.iter().map(keccak256).collect();
        let buckets_root = Tree::new(&leaves, depth_for(leaves.len())).root();

        Ok(BuiltSlice {
            record: SliceRecord {
                level,
                index,
                first_block: first,
                last_block_hash,
                bucket_count,
                addresses: address_count,
                appearances: appearance_count,
                buckets_root,
                roles,
            },
            buckets,
            files,
            spilled,
            paged,
        })
    }

    use crate::addr::layout::{
        UNIT_BLOCKS, decode_bucket_page, decode_spill_page, first_block, pages as split_pages,
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
        assert_eq!(resolve_after(12, 13, &[]), vec![(0, 12)]);
        // Read up to unit 13; at 16 the cover is one slice, re-read whole.
        assert_eq!(resolve_after(13, 16, &[]), vec![(4, 0)]);
        // With (4, 0) pending, only the parts of its expansion after unit 13 are read.
        assert_eq!(
            resolve_after(13, 16, &[(4, 0)]),
            vec![(1, 6), (0, 14), (0, 15)]
        );
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
        let built = build_slice(slice, 0, &apps, B256::repeat_byte(9), 0xdf).unwrap();
        assert_eq!(built.record.bucket_count, 1);
        assert_eq!(built.record.addresses, 2);
        assert_eq!(built.record.appearances, 3);
        assert!(built.files.is_empty());
        let b = BucketRecord::decode(&built.buckets[0], built.record.first_block).unwrap();
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
        // Outside the slice, no roles, or a role the slice does not detect.
        assert!(build_slice(slice, 0, &[app(base + 64, 1, 1, 1)], B256::ZERO, 0xff).is_err());
        assert!(build_slice(slice, 0, &[app(base, 1, 1, 0)], B256::ZERO, 0xff).is_err());
        assert!(build_slice(slice, 0, &[app(base, 1, 1, 0x20)], B256::ZERO, 0xdf).is_err());
        // An empty slice still has one bucket.
        let empty = build_slice((0, 0), 0, &[], B256::ZERO, 0).unwrap();
        assert_eq!(empty.record.bucket_count, 1);
        assert_eq!(empty.buckets.len(), 1);
    }

    /// Builds with the streaming builder, with a sort buffer of `buffer` bytes.
    fn streamed(
        slice: SliceId,
        first_unit: u64,
        apps: &[Appearance],
        roles: u8,
        buffer: usize,
        force: Option<u32>,
    ) -> BuiltSlice {
        let mut b = SliceBuilder::new(slice, first_unit, roles, &std::env::temp_dir(), buffer)
            .unwrap()
            .with_stream_file(usize::MAX);
        for a in apps {
            b.push(a).unwrap();
        }
        let (mut buckets, mut files) = (Vec::new(), Vec::new());
        let built = b
            .finish(B256::repeat_byte(3), force, &mut |out| {
                match out {
                    Output::File(r, f) => files.push((r, f)),
                    Output::Chunk(..) => panic!("streamed with an unlimited threshold"),
                    Output::Bucket(_, p) => buckets.push(p),
                }
                Ok(())
            })
            .unwrap();
        BuiltSlice {
            record: built.record,
            buckets,
            files,
            spilled: built.spilled,
            paged: built.paged,
        }
    }

    fn assert_same(a: &BuiltSlice, b: &BuiltSlice) {
        assert_eq!(a.record, b.record);
        assert_eq!(a.buckets, b.buckets);
        let files = |x: &BuiltSlice| x.files.iter().cloned().collect::<BTreeMap<_, _>>();
        assert_eq!(files(a), files(b));
        assert_eq!(a.files.len(), b.files.len(), "a file listed twice");
        assert_eq!((a.spilled, a.paged), (b.spilled, b.paged));
    }

    /// The streaming builder gives exactly what the in-memory one gave, whether everything
    /// fits one buffer, or takes many runs and merges in rounds; also for a late start and a
    /// forced paged bucket.
    #[test]
    fn streaming_build_matches_the_in_memory_one() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let slice = (4, 2); // units 32..48, blocks 2048..3071
        let (first, last) = (held_first_block(4, 2, 0), last_block(4, 2));
        let mut apps = Vec::new();
        for _ in 0..3000 {
            let r = next();
            // Few addresses get many entries (they spill), most get one or two.
            let address = if r % 7 == 0 {
                Address::repeat_byte((r >> 8) as u8 % 3)
            } else {
                Address::left_padding_from(&(r >> 16).to_be_bytes())
            };
            apps.push(Appearance {
                block: first + (r >> 24) % (last - first + 1),
                slot: (r >> 40) as u32 % 50,
                address,
                roles: 1 << ((r >> 50) % 7),
            });
        }
        let roles = 0x7f;
        let reference =
            reference_build(slice, 0, &apps, B256::repeat_byte(3), roles, None).unwrap();
        assert!(reference.spilled > 0, "the data should spill");
        for buffer in [1 << 20, 4096, ITEM_BYTES * 3] {
            assert_same(&reference, &streamed(slice, 0, &apps, roles, buffer, None));
        }
        // A late start: the slice holds units 40..48.
        let late: Vec<Appearance> = apps
            .iter()
            .filter(|a| a.block >= 40 * UNIT_BLOCKS)
            .cloned()
            .collect();
        let reference =
            reference_build(slice, 40, &late, B256::repeat_byte(3), roles, None).unwrap();
        assert_same(
            &reference,
            &streamed(slice, 40, &late, roles, ITEM_BYTES * 5, None),
        );
        // Everything in one bucket: paged.
        let reference =
            reference_build(slice, 0, &apps, B256::repeat_byte(3), roles, Some(1)).unwrap();
        assert!(reference.paged == 1);
        assert_same(&reference, &streamed(slice, 0, &apps, roles, 4096, Some(1)));
        // An empty slice.
        let reference = reference_build(slice, 0, &[], B256::repeat_byte(3), roles, None).unwrap();
        assert_same(&reference, &streamed(slice, 0, &[], roles, 4096, None));
    }

    /// Bytes of one sort item of the streaming builder.
    const ITEM_BYTES: usize = 45;

    /// More runs than one merge round takes (128 × 128): two rounds, then the last merge.
    #[test]
    fn streaming_build_over_two_merge_rounds() {
        let slice = (6, 1); // blocks 4096..8191
        let apps: Vec<Appearance> = (0..17_000u64)
            .map(|i| Appearance {
                block: 4096 + (i * 7919) % 4096,
                slot: (i % 13) as u32,
                address: Address::left_padding_from(&(i % 3001).to_be_bytes()),
                roles: 1 << (i % 7),
            })
            .collect();
        let reference = reference_build(slice, 0, &apps, B256::repeat_byte(3), 0x7f, None).unwrap();
        assert_same(
            &reference,
            &streamed(slice, 0, &apps, 0x7f, ITEM_BYTES, None),
        );
    }

    /// A build whose cancel flag is set stops with an error and leaves no runs behind.
    #[test]
    fn a_cancelled_build_stops_and_cleans_up() {
        let parent = std::env::temp_dir().join(format!("addr-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&parent).unwrap();
        let mut b = SliceBuilder::new((10, 0), 0, 0x7f, &parent, ITEM_BYTES * 2).unwrap();
        for i in 0..200_000u64 {
            b.push(&Appearance {
                block: i % 65_536,
                slot: 1,
                address: Address::left_padding_from(&i.to_be_bytes()),
                roles: 1,
            })
            .unwrap();
        }
        b.cancel_flag()
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let err = b.finish(B256::ZERO, None, &mut |_| Ok(())).unwrap_err();
        assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
        assert_eq!(
            std::fs::read_dir(&parent).unwrap().count(),
            0,
            "runs left behind"
        );
        std::fs::remove_dir(&parent).unwrap();
    }

    #[test]
    fn slices_of_an_index_from_a_later_unit() {
        // An index from unit 5: slice (3, 0) holds units 5..8, slice (2, 0) does not exist.
        assert_eq!(held((3, 0), 5), 5..8);
        assert_eq!(held((2, 1), 5), 5..8);
        assert_eq!(held((0, 9), 5), 9..10);
        assert!(held((2, 0), 5).is_empty());
        assert_eq!(held((3, 0), 0), 0..8);
        // Sizes as levels: (3, 0) holds 3 units from unit 5, (10, 0) 24 from unit 1000.
        assert_eq!(held_level((3, 0), 5), 2);
        assert_eq!(held_level((3, 0), 0), 3);
        assert_eq!(held_level((10, 0), 1000), 5);
        assert_eq!(held_level((0, 9), 5), 0);
        let apps = vec![app(320, 1, 0x11, 1), app(511, 2, 0x11, 2)];
        let built = build_slice((3, 0), 5, &apps, B256::ZERO, 0xff).unwrap();
        assert_eq!(built.record.first_block, 5 * UNIT_BLOCKS);
        let b = BucketRecord::decode(&built.buckets[0], built.record.first_block).unwrap();
        let BucketBody::Inline(recs) = b.body else {
            panic!("inline expected")
        };
        assert_eq!(
            recs[0].entries,
            Entries::Inline(vec![
                Entry {
                    block: 320,
                    slot: 1,
                    roles: 1
                },
                Entry {
                    block: 511,
                    slot: 2,
                    roles: 2
                },
            ])
        );
        // A block before the start is outside the slice; a slice before it cannot be built.
        assert!(build_slice((3, 0), 5, &[app(319, 1, 0x11, 1)], B256::ZERO, 0xff).is_err());
        assert!(build_slice((2, 0), 5, &[], B256::ZERO, 0xff).is_err());
        // What a client reads for 13 units: the cover's slices that end after the start.
        assert_eq!(resolve_after(5, 13, &[]), vec![(3, 0), (2, 2), (0, 12)]);
        assert_eq!(resolve_after(8, 13, &[]), vec![(2, 2), (0, 12)]);
    }

    #[test]
    fn build_spills_and_pages() {
        let slice = (4, 0); // blocks 0..1024
        let mut apps = Vec::new();
        // One busy address: 400 entries, spilled.
        for b in 0..400 {
            apps.push(app(b, 1, 0xaa, 1));
        }
        // 2000 more addresses with one entry each.
        for i in 0..2000u32 {
            apps.push(Appearance {
                block: (i % 1024) as u64,
                slot: 2,
                address: Address::left_padding_from(&(i + 1_000_000).to_be_bytes()),
                roles: 0x10,
            });
        }
        let built = build_slice(slice, 0, &apps, B256::ZERO, 0x11).unwrap();
        assert_eq!(built.record.addresses, 2001);
        // Every bucket is at most one chunk.
        assert!(built.buckets.iter().all(|b| b.len() <= PAGE_SIZE));
        // The spilled address: its spill file decodes to its 400 entries.
        let busy = address_hash(&Address::repeat_byte(0xaa));
        let k = bucket_of(&busy, built.record.bucket_count) as usize;
        let BucketBody::Inline(recs) =
            BucketRecord::decode(&built.buckets[k], built.record.first_block)
                .unwrap()
                .body
        else {
            panic!("inline expected")
        };
        let rec = recs.iter().find(|r| r.fp == fingerprint(&busy)).unwrap();
        let Entries::Spilled { count, spill_ref } = rec.entries else {
            panic!("spilled expected")
        };
        assert_eq!(count, 400);
        let (_, file) = built.files.iter().find(|(r, _)| *r == spill_ref).unwrap();
        assert_eq!(file_reference(file), spill_ref);
        let entries: Vec<Entry> = split_pages(file)
            .unwrap()
            .iter()
            .flat_map(|p| decode_spill_page(p, 0, 1023).unwrap())
            .collect();
        assert_eq!(entries.len(), 400);
        assert_eq!(entries[399].block, 399);

        // Deterministic: building again gives identical bytes.
        let again = build_slice(slice, 0, &apps, B256::ZERO, 0x11).unwrap();
        assert_eq!(again.buckets, built.buckets);
        assert_eq!(again.files, built.files);
        assert_eq!(again.record, built.record);
    }

    #[test]
    fn identical_spill_files_are_listed_once() {
        // Two addresses with the same 100 entries: one spill file, two spilled addresses.
        let mut apps = Vec::new();
        for b in 0..100 {
            apps.push(app(b * 3, 1, 0xaa, 1));
            apps.push(app(b * 3, 1, 0xbb, 1));
        }
        let built = build_slice((3, 0), 0, &apps, B256::ZERO, 0xff).unwrap();
        assert_eq!(built.spilled, 2);
        assert_eq!(built.files.len(), 1);
        assert_eq!(built.paged, 0);
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
        let built = build(slice, 0, &apps, B256::ZERO, 0x10, Some(1)).unwrap();
        assert_eq!(built.buckets.len(), 1);
        assert_eq!(built.paged, 1);
        assert!(built.buckets[0].len() <= PAGE_SIZE);
        let b = BucketRecord::decode(&built.buckets[0], built.record.first_block).unwrap();
        let BucketBody::Paged {
            count,
            pages,
            pages_ref,
        } = b.body
        else {
            panic!("paged expected")
        };
        assert_eq!(count, 400);
        let (_, file) = built.files.iter().find(|(r, _)| *r == pages_ref).unwrap();
        assert_eq!(file_reference(file), pages_ref);
        let pages_list = split_pages(file).unwrap();
        assert_eq!(pages_list.len(), pages as usize);
        let decoded: Vec<AddressRecord> = pages_list
            .iter()
            .flat_map(|p| decode_bucket_page(p, 0, 63).unwrap())
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
