//! `addr-import`: publishing the address index (`docs/address-index.md`, "Slices and covers"
//! and "Identifiers").
//!
//! The importer keeps its progress in a local state file. Swarm holds what clients read: the
//! slices, one head update per unit, and the origin (a copy of head update 0). Slices are
//! built in memory from the chain, up to `max_level` (part 4b of #8 adds external-memory
//! building for the large levels a mainnet import needs).
//!
//! Heads and the origin are write-once, so they are written ahead: the next head is saved in
//! the state before it is uploaded, and a run that finds a prepared head uploads exactly that
//! record again (signatures are deterministic, so the SOC is byte-identical). Slices are
//! deterministic too and can be uploaded again after a crash.

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
use super::fetch::{FetchedSlice, fetch_slice};
use super::layout::{
    HeadRecord, L_MAX, MAX_UNITS, UNIT_BLOCKS, bucket_id, first_block, head_id, origin_id, slice_id,
};
use super::slice::{BuiltSlice, SliceId, cover, new_slice, units};
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

/// The work for the unit that completes `n` units. Merges of level `defer_level` and above
/// are deferred.
pub fn unit_work(n: u64, defer_level: u8) -> UnitWork {
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

impl Schedule {
    /// Lists `merge` as pending (once, even if a resumed run defers it again).
    pub fn defer(&mut self, merge: SliceId) {
        if !self.pending.contains(&merge) {
            self.pending.push(merge);
        }
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
    /// Units of the last head known to be published.
    pub units: u64,
    pub schedule: Schedule,
    /// Hash of each published unit's last block, as far back as a later slice (a unit's or a
    /// merge's) can start: what that slice must continue or end with.
    pub unit_hashes: BTreeMap<u64, B256>,
    /// The next head, saved before it is uploaded (write-ahead): a run that finds it uploads
    /// exactly this record.
    pub prepared: Option<HeadRecord>,
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
        // Keep what a later slice can still need: the unit before the start of the top-level
        // slice of the newest unit, and before every pending merge (a top-level merge is
        // still pending after the boundary).
        let mut keep = (unit >> L_MAX) << L_MAX;
        for s in &self.schedule.pending {
            keep = keep.min(units(*s).start);
        }
        self.unit_hashes = self.unit_hashes.split_off(&keep.saturating_sub(1));
    }

    fn unit_hash(&self, unit: u64) -> Result<B256> {
        self.unit_hashes
            .get(&unit)
            .copied()
            .with_context(|| format!("no published hash for unit {unit}"))
    }
}

/// Merges built at the same time; the rest wait in a queue.
const MAX_PENDING_MERGES: usize = 16;

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
    traces: bool,
    max_level: u8,
    concurrency: usize,
}

impl<P: Provider + Clone + 'static> Ctx<P> {
    async fn fetch(&self, slice: SliceId, parent: Option<B256>) -> Result<FetchedSlice> {
        ensure!(
            slice.0 <= self.max_level,
            "slice ({}, {}) is above --max-level {}: building it in memory would need too \
             much memory (external-memory building is part 4b of #8)",
            slice.0,
            slice.1,
            self.max_level
        );
        fetch_slice(&self.provider, slice, self.traces, self.concurrency, parent).await
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

    /// Uploads the prepared head of `state`, if any, and records it as published.
    async fn publish_prepared(&self, state: &mut State, dir: &Path) -> Result<()> {
        let Some(head) = state.prepared.clone() else {
            return Ok(());
        };
        self.publish_head(head.units - state.n0, &head).await?;
        state.units = head.units;
        state.prepared = None;
        state.save(dir)?;
        info!(
            units = head.units,
            pending = head.pending.len(),
            "head published"
        );
        Ok(())
    }

    /// Builds and uploads a deferred merge. It must continue the published chain (`before`,
    /// the hash of the unit before it) and end on it (`end`, the hash of its last unit).
    async fn merge(&self, slice: SliceId, end: B256, before: Option<B256>) -> Result<SliceId> {
        let f = self.fetch(slice, before).await?;
        ensure!(
            f.last_hash == end,
            "merge ({}, {}) ends in block {}, not the published {end}",
            slice.0,
            slice.1,
            f.last_hash
        );
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

/// Builds deferred merges one at a time, in the order they were deferred.
struct MergeWorker {
    queue: std::collections::VecDeque<SliceId>,
    running: tokio::task::JoinSet<Result<SliceId>>,
}

impl MergeWorker {
    fn new(queue: Vec<SliceId>) -> Self {
        Self {
            queue: queue.into(),
            running: tokio::task::JoinSet::new(),
        }
    }

    fn len(&self) -> usize {
        self.queue.len() + self.running.len()
    }

    /// Starts the next queued merge if none is running.
    fn kick<P: Provider + Clone + 'static>(&mut self, ctx: &Ctx<P>, state: &State) -> Result<()> {
        if !self.running.is_empty() {
            return Ok(());
        }
        let Some(slice) = self.queue.pop_front() else {
            return Ok(());
        };
        let r = units(slice);
        let end = state.unit_hash(r.end - 1)?;
        let before = match r.start {
            0 => None,
            start => Some(state.unit_hash(start - 1)?),
        };
        let ctx = ctx.clone();
        self.running
            .spawn(async move { ctx.merge(slice, end, before).await });
        Ok(())
    }

    /// Records a finished merge and starts the next. With `wait`, waits for one to finish.
    async fn collect<P: Provider + Clone + 'static>(
        &mut self,
        ctx: &Ctx<P>,
        state: &mut State,
        wait: bool,
    ) -> Result<()> {
        let done = if wait {
            self.running.join_next().await
        } else {
            self.running.try_join_next()
        };
        if let Some(done) = done {
            let slice = done.context("merge task panicked")??;
            state.schedule.merge_done(slice);
        }
        self.kick(ctx, state)
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
    let roles = role::mask(opts.traces);
    let ctx = Ctx {
        provider: provider.clone(),
        bee: bee.clone(),
        signer: signer.clone(),
        batch,
        chain_id,
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
        None => backfill(&ctx, target, roles, dir).await?,
    };
    // A head saved but perhaps not uploaded (the previous run stopped in between).
    ctx.publish_prepared(&mut state, dir).await?;
    info!(
        units = state.units,
        target,
        head = state.head_index(),
        "address index"
    );

    // Merges listed as pending but not built (a previous run stopped): build them again.
    let mut merges = MergeWorker::new(state.schedule.unbuilt());
    merges.kick(&ctx, &state)?;

    for n in state.units + 1..=target {
        merges.collect(&ctx, &mut state, false).await?;
        let (now, deferred) = match unit_work(n, opts.defer_level) {
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
        let start = units(now).start;
        let parent = match start {
            0 => None,
            s => Some(state.unit_hash(s - 1)?),
        };
        let f = ctx.fetch(now, parent).await?;
        ctx.upload_slice(&f.built).await?;
        state.record_unit(n - 1, f.last_hash);
        if let Some(merge) = deferred {
            // Backpressure: not more than MAX_PENDING_MERGES listed at a time.
            while merges.len() >= MAX_PENDING_MERGES {
                merges.collect(&ctx, &mut state, true).await?;
            }
            state.schedule.defer(merge);
            merges.queue.push_back(merge);
            merges.kick(&ctx, &state)?;
        }
        state.prepared = Some(HeadRecord {
            units: n,
            last_block_hash: f.last_hash,
            last_block_timestamp: f.last_timestamp,
            pending: state.schedule.next_head_pending(),
        });
        state.save(dir)?;
        ctx.publish_prepared(&mut state, dir).await?;
    }

    // Let the merges finish. They stay listed until the next run's first head.
    while merges.len() > 0 {
        merges.collect(&ctx, &mut state, true).await?;
        state.save(dir)?;
    }
    Ok(())
}

/// First run: publishes the cover of `[0, target)` (every other past slice would never be
/// read), then head update 0 and the origin, written ahead like every head.
async fn backfill<P: Provider + Clone + 'static>(
    ctx: &Ctx<P>,
    target: u64,
    roles: u8,
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
        roles,
        n0: target,
        units: target,
        schedule: Schedule::default(),
        unit_hashes: BTreeMap::new(),
        prepared: None,
    };
    let (mut parent, mut last) = (None, None);
    for s in slices {
        info!(level = s.0, index = s.1, "backfill");
        let f = ctx.fetch(s, parent).await?;
        ctx.upload_slice(&f.built).await?;
        parent = Some(f.last_hash);
        state.record_unit(units(s).end - 1, f.last_hash);
        last = Some(f);
    }
    let last = last.expect("a cover of at least one unit");
    state.prepared = Some(HeadRecord {
        units: target,
        last_block_hash: last.last_hash,
        last_block_timestamp: last.last_timestamp,
        pending: Vec::new(),
    });
    // From here on the origin's N0 is fixed: a later run publishes this head, never another.
    state.save(dir)?;
    info!(
        units = target,
        "backfill uploaded; publishing head 0 and the origin"
    );
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::slice::resolve;
    use std::collections::BTreeSet;

    #[test]
    fn unit_work_defers_from_the_given_level() {
        assert_eq!(unit_work(13, 2), UnitWork::Build((0, 12)));
        assert_eq!(unit_work(14, 2), UnitWork::Build((1, 6)));
        assert_eq!(
            unit_work(16, 2),
            UnitWork::Defer {
                level0: (0, 15),
                merge: (4, 0)
            }
        );
        // Deferring a merge that is already listed lists it once.
        let mut sched = Schedule::default();
        sched.defer((4, 0));
        sched.defer((4, 0));
        assert_eq!(sched.next_head_pending(), vec![(4, 0)]);
        // Level 0 is never deferred, whatever the threshold.
        assert_eq!(unit_work(3, 0), UnitWork::Build((0, 2)));
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
                    match unit_work(n, defer_level) {
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
            schedule: Schedule {
                pending: vec![(1, 4)],
                built: vec![],
            },
            unit_hashes: BTreeMap::new(),
            prepared: Some(HeadRecord {
                units: 10,
                last_block_hash: B256::repeat_byte(2),
                last_block_timestamp: 1_800_000_000,
                pending: vec![(1, 4)],
            }),
        };
        s.save(&dir).unwrap();
        assert_eq!(State::load(&dir).unwrap(), Some(s.clone()));
        assert_eq!(s.head_index(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(State::load(&dir).unwrap(), None);

        // Unit hashes before the top-level slice of the newest unit are dropped, except the
        // unit just before it, which a merge starting at the boundary checks.
        let top = 1u64 << L_MAX;
        s.schedule = Schedule::default();
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

        // A top-level merge still pending after its boundary keeps what it needs: the unit
        // before its start, and its last unit.
        let mut s2 = s.clone();
        s2.unit_hashes.clear();
        for u in [top - 1, 2 * top - 1] {
            s2.record_unit(u, B256::repeat_byte(1));
        }
        s2.schedule.defer((L_MAX, 1)); // units top..2·top, due at unit 2·top − 1
        for u in 2 * top..2 * top + 100 {
            s2.record_unit(u, B256::repeat_byte(2));
        }
        assert!(s2.unit_hash(top - 1).is_ok(), "the unit before the merge");
        assert!(s2.unit_hash(2 * top - 1).is_ok(), "the merge's last unit");
    }
}
