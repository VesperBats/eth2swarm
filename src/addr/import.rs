//! `addr-import`: publishing the address index (`docs/address-index.md`, "Slices and covers"
//! and "Identifiers").
//!
//! The importer keeps its progress in a local state file. Swarm holds what clients read: the
//! slices, one head update per unit, and the origin (a copy of head update 0). Slices are
//! built in memory from the chain, up to `max_level` (part 4b of #8 adds external-memory
//! building for the large levels a mainnet import needs).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::detect::role;
use super::fetch::fetch_appearances;
use super::layout::{
    HeadRecord, L_MAX, MAX_UNITS, UNIT_BLOCKS, bucket_id, first_block, head_id, last_block,
    origin_id, slice_id,
};
use super::slice::{BuiltSlice, SliceId, build_slice, cover, new_slice, units};
use crate::swarm::{bee::Bee, soc};

/// Name of the state file in the state directory.
pub const STATE_FILE: &str = "addr-import.json";

/// What publishing the unit that completes `n` units requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitWork {
    /// Build and upload this slice before the head.
    Build(SliceId),
    /// Build and upload the unit's level-0 slice before the head, list `merge` as pending in
    /// it, and build `merge` in the background.
    Defer { level0: SliceId, merge: SliceId },
}

/// The deferred merges the importer tracks: listed in heads until they are built, dropped
/// from the first head after that.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// Listed in the last published head.
    pub pending: Vec<SliceId>,
    /// Of those, the ones built and uploaded since.
    pub built: Vec<SliceId>,
}

impl Schedule {
    /// The work for the unit that completes `n` units. Merges of level `defer_level` and above
    /// are deferred.
    pub fn unit_work(&self, n: u64, defer_level: u8) -> UnitWork {
        let s = new_slice(n);
        if s.0 >= defer_level.max(1) {
            UnitWork::Defer {
                level0: (0, (n - 1) as u32),
                merge: s,
            }
        } else {
            UnitWork::Build(s)
        }
    }

    pub fn defer(&mut self, merge: SliceId) {
        self.pending.push(merge);
    }

    pub fn merge_done(&mut self, merge: SliceId) {
        if self.pending.contains(&merge) && !self.built.contains(&merge) {
            self.built.push(merge);
        }
    }

    /// The `pending` list of the next head: every listed merge not built yet, ordered as the
    /// head record requires. The built ones are dropped here.
    pub fn next_head_pending(&mut self) -> Vec<SliceId> {
        let built = std::mem::take(&mut self.built);
        self.pending.retain(|s| !built.contains(s));
        self.pending.sort_by_key(|s| (first_block(s.0, s.1), s.0));
        self.pending.clone()
    }

    /// Listed merges that still have to be built (after a restart, all of them).
    pub fn unbuilt(&self) -> Vec<SliceId> {
        self.pending
            .iter()
            .filter(|s| !self.built.contains(s))
            .copied()
            .collect()
    }
}

/// The importer's progress, kept in [`STATE_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub chain_id: u64,
    pub owner: Address,
    pub roles: u8,
    /// Units of head update 0 (`N0`); update `n` announces `N0 + n` units.
    pub n0: u64,
    /// Units of the last published head.
    pub units: u64,
    pub last_block_hash: B256,
    pub last_block_timestamp: u64,
    pub schedule: Schedule,
    /// Hash of each published unit's last block, since the last top-level boundary: what a
    /// slice built later (a merge) must match.
    pub unit_hashes: BTreeMap<u64, B256>,
}

impl State {
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(STATE_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("reading {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes the state atomically (a temporary file renamed over the old one).
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join(format!("{STATE_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, dir.join(STATE_FILE)).context("replacing the state file")?;
        Ok(())
    }

    /// The head update index of the last published head.
    pub fn head_index(&self) -> u64 {
        self.units - self.n0
    }

    fn record_unit(&mut self, unit: u64, hash: B256) {
        self.unit_hashes.insert(unit, hash);
        // A merge never reaches back past the start of the top-level slice that contains the
        // newest unit; it checks the unit just before its start too.
        let boundary = (unit >> L_MAX) << L_MAX;
        self.unit_hashes = self.unit_hashes.split_off(&boundary.saturating_sub(1));
    }
}

pub struct ImportOptions {
    pub state_dir: PathBuf,
    /// Units to import up to; defaults to every finalized unit.
    pub units: Option<u64>,
    pub traces: bool,
    pub batch: Option<B256>,
    /// Merges of this level and above are built in the background.
    pub defer_level: u8,
    /// Highest level built in memory (part 4b of #8 lifts this limit).
    pub max_level: u8,
    pub concurrency: usize,
}

/// What the importer needs to build and upload slices, cloned into background tasks.
#[derive(Clone)]
struct Ctx<P> {
    provider: P,
    bee: Bee,
    signer: PrivateKeySigner,
    batch: B256,
    chain_id: u64,
    roles: u8,
    traces: bool,
    max_level: u8,
    concurrency: usize,
}

/// A slice fetched from the chain and built: with the links to check it against its
/// neighbours.
struct Fetched {
    built: BuiltSlice,
    first_parent: B256,
    last_hash: B256,
    last_timestamp: u64,
}

impl<P: Provider + Clone + 'static> Ctx<P> {
    async fn fetch_and_build(&self, slice: SliceId) -> Result<Fetched> {
        ensure!(
            slice.0 <= self.max_level,
            "slice ({}, {}) is above --max-level {}: building it in memory would need too \
             much memory (external-memory building is part 4b of #8)",
            slice.0,
            slice.1,
            self.max_level
        );
        let (first, last) = (first_block(slice.0, slice.1), last_block(slice.0, slice.1));
        let mut apps = Vec::new();
        let (mut first_parent, mut last_hash, mut last_timestamp) = (None, B256::ZERO, 0);
        fetch_appearances(
            &self.provider,
            first..=last,
            self.traces,
            self.concurrency,
            |b| {
                first_parent.get_or_insert(b.parent_hash);
                last_hash = b.hash;
                last_timestamp = b.timestamp;
                apps.extend(b.appearances);
                Ok(())
            },
        )
        .await?;
        let built = build_slice(slice, &apps, last_hash, self.roles)?;
        Ok(Fetched {
            built,
            first_parent: first_parent.unwrap_or_default(),
            last_hash,
            last_timestamp,
        })
    }

    /// Uploads a built slice: files first, then the buckets, the descriptor last.
    async fn upload_slice(&self, built: &BuiltSlice) -> Result<()> {
        let (level, index) = (built.record.level, built.record.index);
        let mut set = tokio::task::JoinSet::new();
        let limit = self.concurrency.max(1);
        for (reference, file) in &built.files {
            if set.len() >= limit {
                join_one(&mut set).await?;
            }
            let (bee, batch, file, reference) =
                (self.bee.clone(), self.batch, file.clone(), *reference);
            set.spawn(async move {
                let got = bee.upload_file(&batch, file).await?;
                ensure!(
                    got == reference,
                    "Bee stored a file under {got}, the bucket names {reference}"
                );
                Ok(())
            });
        }
        while !set.is_empty() {
            join_one(&mut set).await?;
        }
        for (k, payload) in built.buckets.iter().enumerate() {
            if set.len() >= limit {
                join_one(&mut set).await?;
            }
            let id = bucket_id(self.chain_id, level, index, k as u32);
            let soc = soc::sign(&self.signer, id, payload.clone())?;
            let (bee, batch, owner) = (self.bee.clone(), self.batch, self.signer.address());
            set.spawn(async move {
                bee.upload_soc(&batch, &owner, &soc).await?;
                Ok(())
            });
        }
        while !set.is_empty() {
            join_one(&mut set).await?;
        }
        self.upload_soc(slice_id(self.chain_id, level, index), built.record.encode())
            .await
    }

    async fn upload_soc(&self, id: B256, payload: Vec<u8>) -> Result<()> {
        let soc = soc::sign(&self.signer, id, payload)?;
        self.bee
            .upload_soc(&self.batch, &self.signer.address(), &soc)
            .await?;
        Ok(())
    }

    async fn publish_head(&self, n: u64, head: &HeadRecord) -> Result<()> {
        let payload = head.encode();
        self.upload_soc(head_id(self.chain_id, n), payload.clone())
            .await?;
        if n == 0 {
            self.upload_soc(origin_id(self.chain_id), payload).await?;
        }
        Ok(())
    }

    /// Builds and uploads a deferred merge, after checking that it ends on the published
    /// chain: its last block is the published hash of its last unit, and its first block's
    /// parent the published hash of the unit before.
    async fn merge(&self, slice: SliceId, end_hash: B256, before: Option<B256>) -> Result<SliceId> {
        let f = self.fetch_and_build(slice).await?;
        ensure!(
            f.last_hash == end_hash,
            "merge ({}, {}) ends in block {}, not the published {end_hash}",
            slice.0,
            slice.1,
            f.last_hash
        );
        if let Some(b) = before {
            ensure!(
                f.first_parent == b,
                "merge ({}, {}) does not continue the published chain",
                slice.0,
                slice.1
            );
        }
        self.upload_slice(&f.built).await?;
        info!(level = slice.0, index = slice.1, "deferred merge uploaded");
        Ok(slice)
    }
}

/// Waits for one upload task, if any is running.
async fn join_one(set: &mut tokio::task::JoinSet<Result<()>>) -> Result<()> {
    match set.join_next().await {
        Some(r) => r.context("upload task panicked")?,
        None => Ok(()),
    }
}

/// Imports up to the target unit count, resuming from the state directory.
pub async fn run<P>(
    provider: &P,
    bee: &Bee,
    signer: &PrivateKeySigner,
    opts: ImportOptions,
) -> Result<()>
where
    P: Provider + Clone + 'static,
{
    ensure!(
        opts.max_level <= L_MAX && opts.defer_level <= L_MAX,
        "levels above {L_MAX}"
    );
    let chain_id = provider.get_chain_id().await.context("querying chain id")?;
    let finalized = provider
        .get_block_by_number(BlockNumberOrTag::Finalized)
        .await
        .context("querying the finalized block")?
        .context("the node reports no finalized block")?
        .header
        .number;
    let complete = (finalized + 1) / UNIT_BLOCKS;
    let target = opts.units.unwrap_or(complete);
    ensure!(
        target <= complete,
        "{target} units requested, but only {complete} are finalized (block {finalized})"
    );
    ensure!(
        target <= MAX_UNITS,
        "{target} units exceed the format's {MAX_UNITS}"
    );
    let batch = match opts.batch {
        Some(b) => b,
        None => bee.usable_batch().await?,
    };
    let roles = if opts.traces { 0xff } else { !role::TRACE };
    let ctx = Ctx {
        provider: provider.clone(),
        bee: bee.clone(),
        signer: signer.clone(),
        batch,
        chain_id,
        roles,
        traces: opts.traces,
        max_level: opts.max_level,
        concurrency: opts.concurrency,
    };
    let dir = &opts.state_dir;

    let mut state = match State::load(dir)? {
        Some(s) => {
            ensure!(
                s.chain_id == chain_id && s.owner == signer.address() && s.roles == roles,
                "the state in {} is for chain {}, owner {}, roles {:#04x}; this run is for \
                 chain {chain_id}, owner {}, roles {roles:#04x}",
                dir.display(),
                s.chain_id,
                s.owner,
                s.roles,
                signer.address()
            );
            s
        }
        None => backfill(&ctx, target, dir).await?,
    };
    info!(
        units = state.units,
        target,
        head = state.head_index(),
        "address index"
    );

    // Merges listed as pending but not built (a previous run stopped): build them again.
    let mut merges = tokio::task::JoinSet::new();
    for slice in state.schedule.unbuilt() {
        spawn_merge(&ctx, &state, slice, &mut merges)?;
    }

    for n in state.units + 1..=target {
        // Merges finished meanwhile are dropped from the next head.
        while let Some(done) = merges.try_join_next() {
            let slice = done.context("merge task panicked")??;
            state.schedule.merge_done(slice);
        }
        let work = state.schedule.unit_work(n, opts.defer_level);
        let (now, deferred) = match work {
            UnitWork::Build(s) => (s, None),
            UnitWork::Defer { level0, merge } => (level0, Some(merge)),
        };
        // A merge listed as pending must be buildable: fail before a head lists it.
        for s in [Some(now), deferred].into_iter().flatten() {
            ensure!(
                s.0 <= opts.max_level,
                "unit {} needs slice ({}, {}), above --max-level {} (external-memory \
                 building is part 4b of #8)",
                n - 1,
                s.0,
                s.1,
                opts.max_level
            );
        }
        let f = ctx.fetch_and_build(now).await?;
        let start = units(now).start;
        if start > 0 {
            let expected = *state
                .unit_hashes
                .get(&(start - 1))
                .with_context(|| format!("no published hash for unit {}", start - 1))?;
            ensure!(
                f.first_parent == expected,
                "unit {}: block {} does not continue the published chain (parent {}, \
                 expected {expected})",
                n - 1,
                first_block(now.0, now.1),
                f.first_parent
            );
        }
        ctx.upload_slice(&f.built).await?;
        state.record_unit(n - 1, f.last_hash);
        if let Some(merge) = deferred {
            state.schedule.defer(merge);
            spawn_merge(&ctx, &state, merge, &mut merges)?;
        }
        let head = HeadRecord {
            units: n,
            last_block_hash: f.last_hash,
            last_block_timestamp: f.last_timestamp,
            pending: state.schedule.next_head_pending(),
        };
        ctx.publish_head(n - state.n0, &head).await?;
        state.units = n;
        state.last_block_hash = f.last_hash;
        state.last_block_timestamp = f.last_timestamp;
        state.save(dir)?;
        info!(units = n, pending = head.pending.len(), "head published");
    }

    // Let running merges finish. They stay listed until the next run's first head.
    while let Some(done) = merges.join_next().await {
        let slice = done.context("merge task panicked")??;
        state.schedule.merge_done(slice);
        state.save(dir)?;
    }
    Ok(())
}

fn spawn_merge<P: Provider + Clone + 'static>(
    ctx: &Ctx<P>,
    state: &State,
    slice: SliceId,
    merges: &mut tokio::task::JoinSet<Result<SliceId>>,
) -> Result<()> {
    let r = units(slice);
    let end_hash = *state
        .unit_hashes
        .get(&(r.end - 1))
        .with_context(|| format!("no published hash for unit {}", r.end - 1))?;
    let before = match r.start {
        0 => None,
        s => Some(
            *state
                .unit_hashes
                .get(&(s - 1))
                .with_context(|| format!("no published hash for unit {}", s - 1))?,
        ),
    };
    let ctx = ctx.clone();
    merges.spawn(async move { ctx.merge(slice, end_hash, before).await });
    Ok(())
}

/// First run: publishes the cover of `[0, target)` (every other past slice would never be
/// read), head update 0 and the origin.
async fn backfill<P: Provider + Clone + 'static>(
    ctx: &Ctx<P>,
    target: u64,
    dir: &Path,
) -> Result<State> {
    ensure!(target >= 1, "nothing to import: no complete unit");
    let slices = cover(target);
    if let Some(s) = slices.iter().find(|s| s.0 > ctx.max_level) {
        bail!(
            "the cover of {target} units contains slice ({}, {}), above --max-level {}; a \
             backfill this long needs external-memory building (part 4b of #8)",
            s.0,
            s.1,
            ctx.max_level
        );
    }
    let mut state = State {
        chain_id: ctx.chain_id,
        owner: ctx.signer.address(),
        roles: ctx.roles,
        n0: target,
        units: target,
        last_block_hash: B256::ZERO,
        last_block_timestamp: 0,
        schedule: Schedule::default(),
        unit_hashes: BTreeMap::new(),
    };
    let mut prev: Option<B256> = None;
    for s in slices {
        info!(level = s.0, index = s.1, "backfill");
        let f = ctx.fetch_and_build(s).await?;
        if let Some(p) = prev {
            ensure!(
                f.first_parent == p,
                "slice ({}, {}) does not continue the previous one",
                s.0,
                s.1
            );
        }
        ctx.upload_slice(&f.built).await?;
        prev = Some(f.last_hash);
        state.record_unit(units(s).end - 1, f.last_hash);
        state.last_block_hash = f.last_hash;
        state.last_block_timestamp = f.last_timestamp;
    }
    let head = HeadRecord {
        units: target,
        last_block_hash: state.last_block_hash,
        last_block_timestamp: state.last_block_timestamp,
        pending: Vec::new(),
    };
    ctx.publish_head(0, &head).await?;
    state.save(dir)?;
    info!(units = target, "backfill published (head 0 and origin)");
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::slice::resolve;
    use std::collections::BTreeSet;

    #[test]
    fn unit_work_defers_from_the_given_level() {
        let s = Schedule::default();
        assert_eq!(s.unit_work(13, 2), UnitWork::Build((0, 12)));
        assert_eq!(s.unit_work(14, 2), UnitWork::Build((1, 6)));
        assert_eq!(
            s.unit_work(16, 2),
            UnitWork::Defer {
                level0: (0, 15),
                merge: (4, 0)
            }
        );
        // Level 0 is never deferred, whatever the threshold.
        assert_eq!(s.unit_work(3, 0), UnitWork::Build((0, 2)));
    }

    /// Simulates the importer: every head must only reference slices that are written, every
    /// pending list must encode, and every merge must eventually leave the heads.
    #[test]
    fn every_head_references_written_slices() {
        for defer_level in [1u8, 2, 3, 5] {
            for delay in [0u64, 1, 2, 5, 17] {
                let n0 = 3;
                let mut written: BTreeSet<SliceId> = cover(n0).into_iter().collect();
                let mut schedule = Schedule::default();
                // (finishes after unit, slice)
                let mut running: Vec<(u64, SliceId)> = Vec::new();
                for n in n0 + 1..=300 {
                    running.retain(|(at, s)| {
                        if *at <= n {
                            written.insert(*s);
                            schedule.merge_done(*s);
                            false
                        } else {
                            true
                        }
                    });
                    match schedule.unit_work(n, defer_level) {
                        UnitWork::Build(s) => {
                            written.insert(s);
                        }
                        UnitWork::Defer { level0, merge } => {
                            written.insert(level0);
                            schedule.defer(merge);
                            // Bigger merges take longer.
                            running.push((n + delay * merge.0 as u64, merge));
                        }
                    }
                    let pending = schedule.next_head_pending();
                    let head = HeadRecord {
                        units: n,
                        last_block_hash: B256::ZERO,
                        last_block_timestamp: 0,
                        pending: pending.clone(),
                    };
                    assert_eq!(HeadRecord::decode(&head.encode()).unwrap(), head);
                    for s in resolve(n, &pending) {
                        assert!(
                            written.contains(&s),
                            "defer {defer_level}, delay {delay}: head {n} reads {s:?}, \
                             not written (pending {pending:?})"
                        );
                    }
                }
                // Once everything finished, the next head lists nothing.
                for (_, s) in running.drain(..) {
                    schedule.merge_done(s);
                }
                assert!(schedule.next_head_pending().is_empty());
            }
        }
    }

    #[test]
    fn state_roundtrip_and_pruning() {
        let dir = std::env::temp_dir().join(format!("addr-import-test-{}", std::process::id()));
        let mut s = State {
            chain_id: 1337,
            owner: Address::repeat_byte(1),
            roles: 0xdf,
            n0: 5,
            units: 9,
            last_block_hash: B256::repeat_byte(2),
            last_block_timestamp: 1_800_000_000,
            schedule: Schedule {
                pending: vec![(1, 4)],
                built: vec![],
            },
            unit_hashes: BTreeMap::new(),
        };
        s.save(&dir).unwrap();
        assert_eq!(State::load(&dir).unwrap(), Some(s.clone()));
        assert_eq!(s.head_index(), 4);
        // Unit hashes before the top-level slice of the newest unit are dropped, except the
        // unit just before it, which a merge starting at the boundary checks. The last unit of
        // a top-level slice still keeps the whole slice (its top-level merge is due then).
        let top = 1u64 << L_MAX;
        for u in [top - 3, top - 2, top - 1] {
            s.record_unit(u, B256::repeat_byte(u as u8));
        }
        assert!(s.unit_hashes.contains_key(&(top - 3)));
        for u in [top, top + 1] {
            s.record_unit(u, B256::repeat_byte(u as u8));
        }
        assert_eq!(
            s.unit_hashes.keys().copied().collect::<Vec<_>>(),
            vec![top - 1, top, top + 1]
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(State::load(&dir).unwrap(), None);
    }
}
