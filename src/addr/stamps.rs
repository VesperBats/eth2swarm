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
use anyhow::{Context, Result, ensure};
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
/// `(i + 2)·2^L`, which is `(⌊i / 2⌋ + 1)·2^(L+1)`. An odd `i` is only written as the
/// level-0 slice of a unit whose merge is deferred, at that unit: the formula gives it no
/// lifetime of its own, and the grace period covers it until the merge is uploaded, as it
/// covers a pending merge's whole expansion.
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

/// Refreshes in a row a lower batch's last known TTL is kept through when Bee cannot be
/// asked (a reset connection, a server error): one failed request must not move a whole
/// slice to a longer batch, whose stamps cannot be taken back.
const KEEP_THROUGH_FAILURES: u32 = 5;

/// What is known about one lower batch: its TTL when last asked, and how many refreshes in
/// a row failed since.
#[derive(Debug, Clone, Copy)]
struct Known {
    at: Instant,
    ttl: u64,
    failures: u32,
}

/// The lower batches' TTLs, and when they were last refreshed.
#[derive(Default)]
struct Ttls {
    refreshed: Option<Instant>,
    known: BTreeMap<B256, Known>,
}

/// The batches of an import or an upload, and which records went where.
pub struct Stamps {
    bee: Bee,
    permanent: B256,
    lower: Vec<B256>,
    grace_secs: u64,
    /// Seconds per unit of this chain.
    unit_secs: u64,
    refresh: Duration,
    /// Held while Bee is asked, so that concurrent callers wait for one refresh.
    ttls: tokio::sync::Mutex<Ttls>,
    used: Mutex<BTreeMap<B256, u64>>,
    fell_back: Mutex<u64>,
}

impl Stamps {
    /// The stamping of a run: `permanent` for permanent records, `lower` for lower slices
    /// (`slot_secs`: the chain's seconds per block, mainnet [`SLOT_SECS`]). Refused if the
    /// permanent batch is also a lower one, or Bee does not know a lower batch (a mistyped
    /// id would otherwise send every lower slice to the permanent batch).
    pub async fn open(
        bee: Bee,
        permanent: B256,
        lower: Vec<B256>,
        grace_days: u64,
        slot_secs: u64,
    ) -> Result<Self> {
        ensure!(
            !lower.contains(&permanent),
            "batch {permanent} is given as the permanent batch and as a lower one"
        );
        let stamps = Self {
            bee,
            permanent,
            lower,
            grace_secs: grace_days.saturating_mul(86_400),
            unit_secs: UNIT_BLOCKS.saturating_mul(slot_secs.max(1)),
            refresh: REFRESH,
            ttls: tokio::sync::Mutex::new(Ttls::default()),
            used: Mutex::new(BTreeMap::new()),
            fell_back: Mutex::new(0),
        };
        for b in &stamps.lower {
            let info = stamps
                .bee
                .batch(b)
                .await
                .with_context(|| format!("reading lower batch {b}"))?
                .with_context(|| format!("Bee knows no batch {b} (--lower-batch)"))?;
            if !info.usable {
                warn!("lower batch {b} is not usable yet: left out until it is");
            }
        }
        Ok(stamps)
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

    /// The lower batches' remaining TTLs (usable ones only), asked at most every refresh
    /// interval and counted down in between. A batch Bee does not know (any more) or that is
    /// not usable is left out; one Bee cannot be asked about keeps its last known TTL through
    /// [`KEEP_THROUGH_FAILURES`] refreshes.
    async fn lower_ttls(&self) -> Vec<(B256, u64)> {
        let mut ttls = self.ttls.lock().await;
        if ttls.refreshed.is_none_or(|at| at.elapsed() >= self.refresh) {
            for b in &self.lower {
                match self.bee.batch(b).await {
                    Ok(Some(info)) if info.usable && info.ttl > 0 => {
                        ttls.known.insert(
                            *b,
                            Known {
                                at: Instant::now(),
                                ttl: info.ttl as u64,
                                failures: 0,
                            },
                        );
                    }
                    Ok(_) => {
                        ttls.known.remove(b);
                    }
                    Err(e) => {
                        let keep = ttls.known.get_mut(b).is_some_and(|k| {
                            k.failures += 1;
                            k.failures <= KEEP_THROUGH_FAILURES
                        });
                        if keep {
                            warn!("lower batch {b}: {e:#}; its last known TTL is kept");
                        } else {
                            ttls.known.remove(b);
                            warn!("lower batch {b} left out: {e:#}");
                        }
                    }
                }
            }
            ttls.refreshed = Some(Instant::now());
        }
        ttls.known
            .iter()
            .map(|(b, k)| (*b, k.ttl.saturating_sub(k.at.elapsed().as_secs())))
            .collect()
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

    /// A local HTTP server answering its n-th connection with `bodies[n]` as JSON (`None`:
    /// a 404, `Some("500")`: a server error).
    fn serve(bodies: Vec<Option<&'static str>>) -> alloy::transports::http::reqwest::Url {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = stream.read(&mut [0u8; 8192]);
                let (status, body) = match body {
                    None => ("404 Not Found", "{}"),
                    Some("500") => ("500 Internal Server Error", "{}"),
                    Some(b) => ("200 OK", b),
                };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        url
    }

    #[tokio::test]
    async fn stamps_choose_by_ttl_keep_through_failures_and_fall_back() {
        let (perm, a, b) = (
            B256::repeat_byte(9),
            B256::repeat_byte(1),
            B256::repeat_byte(2),
        );
        let ttl = |t: u64| -> Option<&'static str> {
            Some(Box::leak(
                format!(r#"{{"usable":true,"batchTTL":{t}}}"#).into_boxed_str(),
            ))
        };
        let bee = Bee::new(serve(vec![
            // open: both known
            ttl(1_000),
            ttl(100_000),
            // refresh 1
            ttl(1_000),
            ttl(100_000),
            // refresh 2: a fails (its TTL is kept), b fine
            Some("500"),
            ttl(100_000),
            // refresh 3: a is gone (left out), b fine
            None,
            ttl(100_000),
            // then nothing answers
        ]))
        .unwrap()
        .with_retries(1);
        // One second per block: a unit is 64 s; no grace.
        let mut s = Stamps::open(bee, perm, vec![a, b], 0, 1).await.unwrap();
        s.refresh = Duration::ZERO;
        // 10 units = 640 s: a (1,000 s) is the batch that expires soonest after it.
        assert_eq!(s.batch_for(Some(10)).await.unwrap(), a);
        assert_eq!(
            s.batch_for(Some(10)).await.unwrap(),
            a,
            "a kept through a failure"
        );
        assert_eq!(s.batch_for(Some(10)).await.unwrap(), b, "a gone");
        // 100 units = 6,400 s: b. Nothing answers any more: b's TTL is kept.
        assert_eq!(s.batch_for(Some(100)).await.unwrap(), b);
        assert_eq!(s.batch_for(None).await.unwrap(), perm);
        // b is kept through KEEP_THROUGH_FAILURES failed refreshes, then left out.
        for _ in 1..KEEP_THROUGH_FAILURES {
            assert_eq!(s.batch_for(Some(10)).await.unwrap(), b);
        }
        assert_eq!(s.batch_for(Some(10)).await.unwrap(), perm);
        assert_eq!(s.fell_back(), 1);
        let used = s.used();
        assert_eq!((used[&a], used[&b], used[&perm]), (2, 6, 2));
    }

    #[tokio::test]
    async fn stamps_refuse_a_mixed_up_or_unknown_batch() {
        let (perm, a) = (B256::repeat_byte(9), B256::repeat_byte(1));
        let bee = Bee::new(serve(vec![None])).unwrap().with_retries(1);
        let err = Stamps::open(bee.clone(), perm, vec![a], 7, 12)
            .await
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("knows no batch"), "{err:#}");
        assert!(Stamps::open(bee, perm, vec![perm], 7, 12).await.is_err());
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
