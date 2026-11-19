//! Postage batches grouped by lifetime (`docs/address-index.md`, "Lifetime"): the top-level
//! slices, the heads and the origin are permanent and go to one batch; a lower slice is dead
//! once the head has passed it (plus a grace period), and goes to the batch that expires
//! soonest after that, so that the batch's cost ends with the slice.
//!
//! The importer never buys or tops up a batch: the operator supplies them (`--batch` for the
//! permanent records, `--lower-batch` for the others), and a lower slice that no lower batch
//! outlives goes to the permanent batch.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy::primitives::B256;
use anyhow::Result;
use tracing::warn;

use super::layout::{L_MAX, UNIT_BLOCKS};
use super::slice::SliceId;
use crate::swarm::bee::Bee;

/// Seconds per block on mainnet: what a unit's duration, and with it a batch's TTL, is
/// measured against unless `--slot-secs` says otherwise.
pub const SLOT_SECS: u64 = 12;

/// The unit at which the head has moved past slice `s` for good, so that no cover uses it any
/// more: `None` for a top-level slice, which is permanent. A lower slice `(L, i)` with `i` even
/// is in the cover until its pair `(L, i + 1)` is complete and the two merge, at unit
/// `(i + 2)·2^L`; one with `i` odd exists only inside a pending merge's expansion, which ends
/// with that same merge at `(i + 1)·2^L`. Both are `(⌊i / 2⌋ + 1)·2^(L+1)`. A pending merge
/// keeps its expansion alive while it is built: the grace period covers that.
pub fn dead_at(s: SliceId) -> Option<u64> {
    let (level, index) = s;
    (level < L_MAX).then(|| ((index as u64 >> 1) + 1) << (level + 1))
}

/// Units a record of slice `s` must still live, written while the index has `now` units:
/// `None` for a permanent one.
pub fn life_units(s: SliceId, now: u64) -> Option<u64> {
    dead_at(s).map(|d| d.saturating_sub(now))
}

/// The batch for a record that must live `need` more seconds, among lower batches with their
/// remaining TTL in seconds: the one that expires soonest after it. `None` when none lives
/// long enough.
pub fn choose(batches: &[(B256, u64)], need: u64) -> Option<B256> {
    batches
        .iter()
        .filter(|(_, ttl)| *ttl >= need)
        .min_by_key(|(id, ttl)| (*ttl, *id))
        .map(|(id, _)| *id)
}

/// Default days a lower batch must outlive the slices it stamps.
pub const GRACE_DAYS: u64 = 7;

/// How long a batch's TTL is trusted before it is asked again (it counts down, and the
/// price can change).
const REFRESH: Duration = Duration::from_secs(60);

/// The lower batches' TTLs in seconds, and when Bee was asked.
type Ttls = (Instant, Vec<(B256, u64)>);

/// The batches of an import or an upload, and which records went where.
pub struct Stamps {
    bee: Bee,
    permanent: B256,
    lower: Vec<B256>,
    grace_secs: u64,
    /// Seconds per unit of this chain.
    unit_secs: u64,
    /// Held while Bee is asked, so that concurrent callers wait for one refresh.
    ttls: tokio::sync::Mutex<Option<Ttls>>,
    used: Mutex<BTreeMap<B256, u64>>,
    fell_back: Mutex<u64>,
}

impl Stamps {
    /// `slot_secs` is the chain's seconds per block (mainnet: [`SLOT_SECS`]).
    pub fn new(
        bee: Bee,
        permanent: B256,
        lower: Vec<B256>,
        grace_days: u64,
        slot_secs: u64,
    ) -> Self {
        Self {
            bee,
            permanent,
            lower,
            grace_secs: grace_days.saturating_mul(86_400),
            unit_secs: UNIT_BLOCKS.saturating_mul(slot_secs.max(1)),
            ttls: tokio::sync::Mutex::new(None),
            used: Mutex::new(BTreeMap::new()),
            fell_back: Mutex::new(0),
        }
    }

    /// The batch for a record that must live `life` more units (`None`: permanently).
    pub async fn batch_for(&self, life: Option<u64>) -> Result<B256> {
        let batch = match life {
            None => self.permanent,
            Some(_) if self.lower.is_empty() => self.permanent,
            Some(units) => {
                let need = units
                    .saturating_mul(self.unit_secs)
                    .saturating_add(self.grace_secs);
                match choose(&self.lower_ttls().await, need) {
                    Some(b) => b,
                    None => {
                        let mut n = self.fell_back.lock().unwrap();
                        if *n == 0 {
                            warn!(
                                "no --lower-batch lives {need} s more (a slice's {units} units \
                                 plus the grace period): such slices go to the permanent \
                                 batch {}",
                                self.permanent
                            );
                        }
                        *n += 1;
                        self.permanent
                    }
                }
            }
        };
        *self.used.lock().unwrap().entry(batch).or_default() += 1;
        Ok(batch)
    }

    /// The lower batches' remaining TTLs (usable ones only), asked at most every
    /// [`REFRESH`] and counted down in between. A batch Bee cannot tell about (expired and
    /// gone, or a failed request) is left out until the next refresh: its slices go to
    /// another batch, or the permanent one.
    async fn lower_ttls(&self) -> Vec<(B256, u64)> {
        let mut cache = self.ttls.lock().await;
        if let Some((at, ttls)) = &*cache
            && at.elapsed() < REFRESH
        {
            let gone = at.elapsed().as_secs();
            return ttls
                .iter()
                .map(|(b, t)| (*b, t.saturating_sub(gone)))
                .collect();
        }
        let mut ttls = Vec::with_capacity(self.lower.len());
        for b in &self.lower {
            match self.bee.batch(b).await {
                Ok(info) if info.usable && info.ttl > 0 => ttls.push((*b, info.ttl as u64)),
                Ok(_) => {}
                Err(e) => warn!("lower batch {b} left out: {e:#}"),
            }
        }
        *cache = Some((Instant::now(), ttls.clone()));
        ttls
    }

    /// Records (slice descriptors, buckets, files, heads) per batch, for the summary.
    pub fn used(&self) -> BTreeMap<B256, u64> {
        self.used.lock().unwrap().clone()
    }

    /// Lower-slice records that went to the permanent batch because no lower batch lived
    /// long enough.
    pub fn fell_back(&self) -> u64 {
        *self.fell_back.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_die_when_the_head_passes_them() {
        // (0, 12) is in the cover at N = 13 and merges with (0, 13) at 14.
        assert_eq!(dead_at((0, 12)), Some(14));
        // (0, 13) exists only in a pending merge's expansion, which ends at 14 too.
        assert_eq!(dead_at((0, 13)), Some(14));
        // (2, 2) holds units 8..12: in the cover for N in 12..16.
        assert_eq!(dead_at((2, 2)), Some(16));
        // The top level is permanent.
        assert_eq!(dead_at((L_MAX, 3)), None);
        // A slice of level 15 lives 2^15 units (about 291 days) after it ends.
        assert_eq!(dead_at((15, 0)).unwrap() - (1 << 15), 1 << 15);
        assert_eq!(life_units((2, 2), 12), Some(4));
        assert_eq!(life_units((2, 2), 20), Some(0));
        assert_eq!(life_units((L_MAX, 0), 70_000), None);
    }

    #[test]
    fn the_batch_that_expires_soonest_after_the_slice() {
        let (a, b, c) = (
            B256::repeat_byte(1),
            B256::repeat_byte(2),
            B256::repeat_byte(3),
        );
        let batches = [(a, 1_000), (b, 50_000), (c, 20_000)];
        assert_eq!(choose(&batches, 500), Some(a));
        assert_eq!(choose(&batches, 1_000), Some(a));
        assert_eq!(choose(&batches, 1_001), Some(c));
        assert_eq!(choose(&batches, 30_000), Some(b));
        assert_eq!(choose(&batches, 50_001), None);
        assert_eq!(choose(&[], 1), None);
    }
}
