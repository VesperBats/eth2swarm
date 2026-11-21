//! Sizing and costing postage batches (`ethswarm stamps plan`; `.claude/agent/postage.md`).
//!
//! A batch of depth `d` holds 2^d chunks in 2^16 buckets, a chunk's bucket being the first 16
//! bits of its address. Bee refuses (immutable) or overwrites (mutable) once one bucket is
//! full, so what decides the depth is the load of the fullest bucket, not the total. An export
//! holds every chunk address it will upload, so that load is known before anything is sent.
//!
//! A batch's lifetime is its balance per chunk (`amount`, in PLUR) divided by the storage
//! price (PLUR per chunk per block, `GET /chainstate`); the batch costs `amount · 2^d` PLUR.
//! 1 BZZ is 10^16 PLUR. The price is set by the chain and moves: a cost or a lifetime computed
//! from today's price is an estimate.

use alloy::primitives::B256;

/// Bucket depth of every Swarm postage batch.
pub const BUCKET_DEPTH: u8 = 16;
/// The smallest depth Bee sells (a bucket holds two chunks).
pub const MIN_DEPTH: u8 = BUCKET_DEPTH + 1;
/// Seconds per block of Gnosis Chain, where the postage contract lives.
pub const GNOSIS_BLOCK_SECS: u64 = 5;
/// PLUR in one BZZ.
pub const PLUR_PER_BZZ: u128 = 10_000_000_000_000_000;

/// The bucket of a chunk address.
pub fn bucket(address: &B256) -> usize {
    u16::from_be_bytes([address[0], address[1]]) as usize
}

/// Chunks per bucket.
#[derive(Debug, Clone)]
pub struct BucketLoad {
    counts: Vec<u32>,
    chunks: u64,
}

impl Default for BucketLoad {
    fn default() -> Self {
        Self {
            counts: vec![0; 1 << BUCKET_DEPTH],
            chunks: 0,
        }
    }
}

impl BucketLoad {
    pub fn add(&mut self, address: &B256) {
        self.counts[bucket(address)] += 1;
        self.chunks += 1;
    }

    /// Chunks in each bucket, by its index.
    pub fn counts(&self) -> &[u32] {
        &self.counts
    }

    /// Chunks counted.
    pub fn chunks(&self) -> u64 {
        self.chunks
    }

    /// Chunks in the fullest bucket.
    pub fn fullest(&self) -> u64 {
        self.counts.iter().copied().max().unwrap_or(0).into()
    }

    /// Mean chunks per bucket.
    pub fn mean(&self) -> f64 {
        self.chunks as f64 / self.counts.len() as f64
    }

    /// The smallest depth whose buckets hold the fullest one at no more than `max_fill`
    /// (0 < max_fill ≤ 1) of their capacity, and at least [`MIN_DEPTH`]; `None` if none does.
    pub fn depth_for(&self, max_fill: f64) -> Option<u8> {
        depth_for(self.fullest(), max_fill)
    }
}

/// Chunks one bucket of a depth-`depth` batch holds.
pub fn bucket_capacity(depth: u8) -> u64 {
    1u64 << depth.saturating_sub(BUCKET_DEPTH).min(63)
}

/// The deepest batch [`depth_for`] proposes (2^48 chunks per bucket: far beyond any index).
pub const MAX_DEPTH: u8 = 64;

/// The smallest depth (at least [`MIN_DEPTH`]) whose buckets hold `fullest` chunks at no more
/// than `max_fill` of their capacity; `None` if none up to [`MAX_DEPTH`] does.
pub fn depth_for(fullest: u64, max_fill: f64) -> Option<u8> {
    let max_fill = max_fill.clamp(f64::MIN_POSITIVE, 1.0);
    (MIN_DEPTH..=MAX_DEPTH).find(|d| fullest as f64 <= bucket_capacity(*d) as f64 * max_fill)
}

/// The balance per chunk (PLUR) that keeps a batch alive `secs` seconds at `price` PLUR per
/// chunk per block, with blocks of `block_secs` seconds.
pub fn amount_for(price: u128, secs: u64, block_secs: u64) -> u128 {
    price.saturating_mul(u128::from(secs.div_ceil(block_secs.max(1))))
}

/// What a batch of `depth` with balance `amount` per chunk costs, in PLUR.
pub fn cost_plur(amount: u128, depth: u8) -> u128 {
    amount.saturating_mul(1u128 << depth.min(100))
}

/// PLUR as BZZ with four decimals (rounded up, so a quoted cost is never short).
pub fn format_bzz(plur: u128) -> String {
    let unit = PLUR_PER_BZZ / 10_000;
    let tenths = plur.div_ceil(unit);
    format!("{}.{:04} BZZ", tenths / 10_000, tenths % 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b0: u8, b1: u8, rest: u8) -> B256 {
        let mut a = B256::repeat_byte(rest);
        a.0[0] = b0;
        a.0[1] = b1;
        a
    }

    #[test]
    fn buckets_are_the_first_16_bits() {
        assert_eq!(bucket(&addr(0, 0, 0xff)), 0);
        assert_eq!(bucket(&addr(0x12, 0x34, 0)), 0x1234);
        assert_eq!(bucket(&addr(0xff, 0xff, 0)), 0xffff);
        let mut load = BucketLoad::default();
        for rest in 0..3 {
            load.add(&addr(0x12, 0x34, rest));
        }
        load.add(&addr(0, 1, 0));
        assert_eq!((load.chunks(), load.fullest()), (4, 3));
    }

    #[test]
    fn depth_follows_the_fullest_bucket() {
        assert_eq!(bucket_capacity(17), 2);
        assert_eq!(bucket_capacity(23), 128);
        // The mainnet month index (2026-11-20): fullest bucket 32 chunks. Full at depth 21,
        // half full at 22.
        assert_eq!(depth_for(32, 1.0), Some(21));
        assert_eq!(depth_for(32, 0.5), Some(22));
        assert_eq!(depth_for(33, 0.5), Some(23));
        // Never below what Bee sells.
        assert_eq!(depth_for(0, 0.5), Some(MIN_DEPTH));
        assert_eq!(depth_for(1, 1.0), Some(MIN_DEPTH));
        assert_eq!(depth_for(u64::MAX, 0.5), None, "nothing holds it");
    }

    #[test]
    fn cost_and_lifetime() {
        // 24,000 PLUR per chunk per block, 30 days of 5 s blocks: 518,400 blocks.
        let amount = amount_for(24_000, 30 * 86_400, GNOSIS_BLOCK_SECS);
        assert_eq!(amount, 24_000 * 518_400);
        // A started block counts.
        assert_eq!(amount_for(10, 6, 5), 20);
        let cost = cost_plur(amount, 22);
        assert_eq!(cost, 24_000 * 518_400 * (1 << 22));
        assert_eq!(format_bzz(cost), "5.2184 BZZ");
        assert_eq!(format_bzz(PLUR_PER_BZZ), "1.0000 BZZ");
        assert_eq!(format_bzz(1), "0.0001 BZZ", "rounded up");
        assert_eq!(format_bzz(0), "0.0000 BZZ");
    }
}
