//! `addr-stats`: what the address index costs, measured with the real encoder.
//!
//! A window is one slice of level `L`. Every slice of every level `0..=L` inside it is built
//! in memory (nothing is uploaded) and summed up per level: the figures behind the
//! measurement tables of `docs/address-index.md`.

use anyhow::{Result, ensure};

use super::detect::{Appearance, role};
use super::layout::{BucketBody, BucketRecord, first_block, last_block};
use super::slice::{SliceId, build_slice};
use crate::swarm::file::file_chunks;

/// Sums over the slices of one level in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LevelStats {
    pub level: u8,
    pub slices: u64,
    pub addresses: u64,
    pub appearances: u64,
    pub buckets: u64,
    pub paged_buckets: u64,
    pub spilled_addresses: u64,
    /// Bucket payloads plus spill and pages files.
    pub bytes: u64,
    /// Descriptor, bucket SOCs, and every chunk of the spill and pages files.
    pub chunks: u64,
    pub largest_bucket: u64,
}

/// One window: its appearance mix and the per-level sums.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowStats {
    pub first_block: u64,
    pub last_block: u64,
    pub txs: u64,
    pub appearances: u64,
    /// Appearances whose only role is `WORD`.
    pub word_only: u64,
    /// Appearances with the `TRACE` role.
    pub with_trace: u64,
    pub levels: Vec<LevelStats>,
}

/// Builds every slice of levels `0..=window.0` inside `window` from its appearances
/// (`appearances` sorted by block) and sums them up per level.
pub fn window_stats(
    window: SliceId,
    appearances: &[Appearance],
    txs: u64,
    roles: u8,
) -> Result<WindowStats> {
    let (top, index) = window;
    let (first, last) = (first_block(top, index), last_block(top, index));
    ensure!(
        appearances.windows(2).all(|w| w[0].block <= w[1].block),
        "appearances not sorted by block"
    );
    ensure!(
        appearances
            .iter()
            .all(|a| (first..=last).contains(&a.block)),
        "appearance outside window blocks {first}..={last}"
    );
    let mut out = WindowStats {
        first_block: first,
        last_block: last,
        txs,
        appearances: appearances.len() as u64,
        word_only: appearances.iter().filter(|a| a.roles == role::WORD).count() as u64,
        with_trace: appearances
            .iter()
            .filter(|a| a.roles & role::TRACE != 0)
            .count() as u64,
        levels: Vec::new(),
    };
    for level in 0..=top {
        let mut s = LevelStats {
            level,
            ..Default::default()
        };
        let per_window = 1u32 << (top - level);
        for k in 0..per_window {
            let slice = (level, index * per_window + k);
            let (a, b) = (first_block(slice.0, slice.1), last_block(slice.0, slice.1));
            let lo = appearances.partition_point(|x| x.block < a);
            let hi = appearances.partition_point(|x| x.block <= b);
            // The hash of the last block is not needed for sizes.
            let built = build_slice(slice, &appearances[lo..hi], Default::default(), roles)?;
            let paged = built
                .buckets
                .iter()
                .filter(|b| {
                    matches!(
                        BucketRecord::decode(b).map(|b| b.body),
                        Ok(BucketBody::Paged { .. })
                    )
                })
                .count() as u64;
            s.slices += 1;
            s.addresses += built.record.addresses as u64;
            s.appearances += built.record.appearances;
            s.buckets += built.buckets.len() as u64;
            s.paged_buckets += paged;
            // Every file is a spill file, except one pages file per paged bucket.
            s.spilled_addresses += built.files.len() as u64 - paged;
            s.bytes += built.buckets.iter().map(|b| b.len() as u64).sum::<u64>()
                + built.files.iter().map(|(_, f)| f.len() as u64).sum::<u64>();
            s.chunks += 1
                + built.buckets.len() as u64
                + built
                    .files
                    .iter()
                    .map(|(_, f)| file_chunks(f.len()) as u64)
                    .sum::<u64>();
            s.largest_bucket = s.largest_bucket.max(
                built
                    .buckets
                    .iter()
                    .map(|b| b.len() as u64)
                    .max()
                    .unwrap_or(0),
            );
        }
        out.levels.push(s);
    }
    Ok(out)
}

/// The window as a Markdown table, one row per level, averages per slice.
pub fn render(w: &WindowStats) -> String {
    let blocks = w.last_block - w.first_block + 1;
    let pct = |n: u64| 100.0 * n as f64 / w.appearances.max(1) as f64;
    let mut out = format!(
        "Blocks {}..={}: {:.1} txs and {:.1} appearances per block, `WORD` only {:.1} %, \
         with `TRACE` {:.1} %\n\n",
        w.first_block,
        w.last_block,
        w.txs as f64 / blocks as f64,
        w.appearances as f64 / blocks as f64,
        pct(w.word_only),
        pct(w.with_trace),
    );
    out.push_str(
        "| Level | Blocks / slice | Slices | Addresses / slice | Appearances / slice | \
         Bytes / appearance | Buckets / slice | Paged | Spilled / slice | Chunks / slice | \
         Chunks / 1000 appearances | Largest bucket |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for s in &w.levels {
        let n = s.slices.max(1) as f64;
        out.push_str(&format!(
            "| {} | {} | {} | {:.0} | {:.0} | {:.2} | {:.1} | {} | {:.1} | {:.1} | {:.2} | {} |\n",
            s.level,
            blocks / s.slices.max(1),
            s.slices,
            s.addresses as f64 / n,
            s.appearances as f64 / n,
            s.bytes as f64 / s.appearances.max(1) as f64,
            s.buckets as f64 / n,
            s.paged_buckets,
            s.spilled_addresses as f64 / n,
            s.chunks as f64 / n,
            1000.0 * s.chunks as f64 / s.appearances.max(1) as f64,
            s.largest_bucket,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    fn apps(window: SliceId) -> Vec<Appearance> {
        let (first, last) = (
            first_block(window.0, window.1),
            last_block(window.0, window.1),
        );
        let mut v = Vec::new();
        for b in first..=last {
            // A busy address in every block, and a fresh one per block.
            v.push(Appearance {
                block: b,
                slot: 1,
                address: Address::repeat_byte(0xaa),
                roles: role::FROM,
            });
            v.push(Appearance {
                block: b,
                slot: 1,
                address: Address::left_padding_from(&(b + 100_000).to_be_bytes()),
                roles: role::WORD,
            });
        }
        v
    }

    #[test]
    fn levels_cover_the_window() {
        let window = (2, 5); // 4 units, 256 blocks
        let a = apps(window);
        let w = window_stats(window, &a, 300, 0xff).unwrap();
        assert_eq!(w.appearances, 512);
        assert_eq!(w.word_only, 256);
        assert_eq!(w.with_trace, 0);
        assert_eq!(w.levels.len(), 3);
        for (s, slices) in w.levels.iter().zip([4u64, 2, 1]) {
            assert_eq!(s.slices, slices);
            // Every level holds the same appearances, grouped differently.
            assert_eq!(s.appearances, 512);
            assert!(
                s.chunks >= s.slices * 2,
                "descriptor and at least one bucket"
            );
        }
        // The busy address has 64 entries per unit: inline at level 0 is at most 256 bytes,
        // so whether it spills depends on the slice length.
        assert_eq!(w.levels[0].addresses, 4 * 65);
        assert_eq!(w.levels[2].addresses, 257);
        assert_eq!(w.levels[2].spilled_addresses, 1);
        let table = render(&w);
        assert!(table.contains("| 2 | 256 | 1 | 257 | 512 |"), "{table}");
    }

    #[test]
    fn rejects_unsorted_or_outside() {
        let window = (0, 1);
        let mut a = apps(window);
        a.swap(0, 5);
        assert!(window_stats(window, &a, 0, 0xff).is_err());
        assert!(window_stats((0, 0), &apps(window), 0, 0xff).is_err());
    }
}
