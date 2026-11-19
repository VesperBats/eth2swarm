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

use super::build::{Output, SliceBuilder};
use super::detect::role;
use super::export::{self, ExportLog, ExportMeta};
use super::fetch::fetch_appearances;
use super::io::{join_one, write_atomic};
use super::layout::{
    HeadRecord, L_MAX, MAX_UNITS, UNIT_BLOCKS, bucket_id, first_block, head_id, held_first_block,
    last_block, origin_id, slice_id,
};
use super::slice::{SliceId, cover, held, held_level, new_slice, resolve_after, units};
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

    /// Records a built merge. Pending merges inside it are done too: once it is dropped from
    /// the heads, nothing reads them (a resumed run can have both listed).
    pub fn merge_done(&mut self, merge: SliceId) {
        let outer = units(merge);
        for s in &self.pending {
            let inner = units(*s);
            let inside = outer.start <= inner.start && inner.end <= outer.end;
            if inside && !self.built.contains(s) {
                self.built.push(*s);
            }
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

    /// Listed merges that still have to be built, in the order they were deferred (by their
    /// last unit; the list itself is in head order).
    pub fn unbuilt(&self) -> Vec<SliceId> {
        let mut out: Vec<SliceId> = self
            .pending
            .iter()
            .filter(|s| !self.built.contains(s))
            .copied()
            .collect();
        out.sort_by_key(|s| units(*s).end);
        out
    }
}

/// The importer's progress, kept in [`STATE_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub chain_id: u64,
    pub owner: Address,
    pub roles: u8,
    /// The first unit the index covers (`S`, 0 from genesis), fixed by the first run.
    pub first_unit: u64,
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
    /// For an index exported to a directory (`--out`) instead of uploaded: the length of its
    /// log when this state was saved. A resumed run truncates the log to it.
    #[serde(default)]
    pub export_len: Option<u64>,
    /// A backfill of `n0` units from `first_unit` has started and not finished. Saved before
    /// its first upload, so that a run after a failure repeats exactly that backfill: a slice
    /// the start falls into depends on the start, and its identifiers do not.
    #[serde(default)]
    pub backfilling: bool,
}

impl State {
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(STATE_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "reading {} (a state without `first_unit` was written before an index \
                     could start at a later block; its heads use the earlier format, so that \
                     index cannot be continued: start a new one under another key)",
                    path.display()
                )
            })?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes the state durably: a temporary file, synced, renamed over the old one, and the
    /// directory synced, so that after a crash or a power loss the file is either the old or
    /// the new state, and a head saved before its upload stays saved.
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        write_atomic(&dir.join(STATE_FILE), &serde_json::to_vec_pretty(self)?)
    }

    /// The head update index of the last published head.
    pub fn head_index(&self) -> u64 {
        self.units - self.n0
    }

    /// Drops the unit hashes no later slice can need once `n` units are published: only the
    /// last unit of each slice of the cover of `[0, n)` (where the next slices start) and both
    /// ends of every pending merge are ever read.
    fn prune(&mut self, n: u64) {
        let mut keep: std::collections::BTreeSet<u64> =
            cover(n).iter().map(|s| units(*s).end - 1).collect();
        for s in &self.schedule.pending {
            let r = held(*s, self.first_unit);
            keep.insert(r.end - 1);
            if r.start > self.first_unit {
                keep.insert(r.start - 1);
            }
        }
        self.unit_hashes.retain(|u, _| keep.contains(u));
    }

    /// The hash a slice holding units from `start` on must continue: the published unit before
    /// it, or none for a slice that starts with the index.
    fn parent_of(&self, start: u64) -> Result<Option<B256>> {
        if start == self.first_unit {
            Ok(None)
        } else {
            self.unit_hash(start - 1).map(Some)
        }
    }

    fn unit_hash(&self, unit: u64) -> Result<B256> {
        self.unit_hashes
            .get(&unit)
            .copied()
            .with_context(|| format!("no published hash for unit {unit}"))
    }
}

/// Deferred merges listed as pending at a time, queued or running (they are built one at a
/// time). At this limit the importer waits for one to finish before deferring another.
const MAX_PENDING_MERGES: usize = 16;

/// Holds the state directory for one run: a second importer on the same directory would
/// write the same heads from a diverging state.
pub struct StateLock(#[allow(dead_code)] std::fs::File);

impl StateLock {
    pub fn acquire(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("addr-import.lock");
        let f =
            std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        f.try_lock().map_err(|_| {
            anyhow::anyhow!(
                "another addr-import holds {} (one importer per state directory)",
                path.display()
            )
        })?;
        Ok(Self(f))
    }
}

/// Test hook: `ETH2SWARM_ADDR_FAILPOINT=before-head-upload` or `after-head-upload` makes a run
/// stop around a head upload, `backfill-slice` after the first slice of a backfill, as a crash
/// would (used by `scripts/e2e-bee-factory.sh`).
fn failpoint(name: &str) -> Result<()> {
    match std::env::var("ETH2SWARM_ADDR_FAILPOINT") {
        Ok(v) if v == name => bail!("failpoint {name}"),
        _ => Ok(()),
    }
}

pub struct ImportOptions {
    pub state_dir: PathBuf,
    /// Units to import up to; defaults to every finalized unit.
    pub units: Option<u64>,
    /// Keep importing newly finalized units (with deferred merges built meanwhile).
    pub follow: bool,
    /// A first run: start a new index. Without it, a missing state file is an error.
    pub init: bool,
    /// The first run's start: the index covers the unit of this block and every later one
    /// (default: from genesis). A later run must state the same unit or nothing.
    pub from_block: Option<u64>,
    pub traces: bool,
    pub batch: Option<B256>,
    /// Write to this directory instead of uploading (`addr-upload` uploads it later).
    pub out: Option<PathBuf>,
    /// Merges of this level and above are built in the background.
    pub defer_level: u8,
    /// Highest slice level the importer builds (a cap on the work one slice may take).
    pub max_level: u8,
    pub concurrency: usize,
    /// Where slices keep their sort runs (default: `sort` in the state directory).
    pub sort_dir: Option<PathBuf>,
    /// Bytes of one sort buffer.
    pub sort_buffer: usize,
}

/// Where the importer's records go: a Bee node, or an export directory's log, written in
/// the order they would be uploaded.
#[derive(Clone)]
enum Sink {
    Bee { bee: Bee, batch: B256 },
    Dir(std::sync::Arc<std::sync::Mutex<ExportLog>>),
}

fn lock(log: &std::sync::Mutex<ExportLog>) -> Result<std::sync::MutexGuard<'_, ExportLog>> {
    log.lock()
        .map_err(|_| anyhow::anyhow!("an export write panicked"))
}

/// What the importer needs to build and upload slices, cloned into background tasks.
#[derive(Clone)]
struct Ctx<P> {
    provider: P,
    sink: Sink,
    signer: PrivateKeySigner,
    chain_id: u64,
    /// The first unit the index covers.
    first_unit: u64,
    traces: bool,
    max_level: u8,
    concurrency: usize,
    /// Where slices keep their sort runs while they are built.
    sort_dir: PathBuf,
    /// Held for the run: one importer per sort directory.
    _sort_lock: std::sync::Arc<std::fs::File>,
    /// Bytes of one sort buffer (a background merge and a unit can each hold one).
    sort_buffer: usize,
}

/// Sets a build's cancel flag when dropped.
struct CancelOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The last block of a published slice.
struct Published {
    last_hash: B256,
    last_timestamp: u64,
}

impl<P: Provider + Clone + 'static> Ctx<P> {
    /// Builds slice `slice` and publishes it as it is built: its blocks go into a
    /// [`SliceBuilder`] (sort runs on disk under the sort directory), then every file and
    /// bucket goes to the sink as it is made, the descriptor last. With `parent`, the slice's
    /// first block must be its child; with `end`, its last block must be that one. Both are
    /// checked before anything is published.
    async fn publish(
        &self,
        slice: SliceId,
        parent: Option<B256>,
        end: Option<B256>,
    ) -> Result<Published> {
        check_level(slice, self.first_unit, self.max_level)?;
        let (level, index) = slice;
        let mut builder = SliceBuilder::new(
            slice,
            self.first_unit,
            role::mask(self.traces),
            &self.sort_dir,
            self.sort_buffer,
        )?;
        let (first, last) = (
            held_first_block(level, index, self.first_unit),
            last_block(level, index),
        );
        // However publishing ends, a build still running stops at its next look.
        let _cancel = CancelOnDrop(builder.cancel_flag());
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
                b.appearances.iter().try_for_each(|a| {
                    if builder.push_spills() {
                        // Sorting and writing a full buffer takes seconds: off the runtime.
                        tokio::task::block_in_place(|| builder.push(a))
                    } else {
                        builder.push(a)
                    }
                })
            },
        )
        .await?;
        if let (Some(expected), Some(got)) = (parent, first_parent) {
            ensure!(
                got == expected,
                "slice ({level}, {index}): block {first}'s parent is {got}, not the published \
                 {expected} (reorg or another chain?)"
            );
        }
        if let Some(end) = end {
            ensure!(
                last_hash == end,
                "slice ({level}, {index}) ends in block {last_hash}, not the published {end}"
            );
        }

        // The build runs on a blocking thread and hands out files and buckets through a
        // channel; they are signed and sent here as they come.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Output>(256);
        let build = tokio::task::spawn_blocking(move || {
            builder.finish(last_hash, None, &mut |out| {
                tx.blocking_send(out)
                    .map_err(|_| anyhow::anyhow!("publishing stopped"))
            })
        });
        let owner = self.signer.address();
        let mut set = tokio::task::JoinSet::new();
        let limit = self.concurrency.max(1);
        while let Some(out) = rx.recv().await {
            match (&self.sink, out) {
                (Sink::Dir(log), Output::File(reference, file)) => {
                    lock(log)?.append(export::FILE, &reference, &file)?
                }
                (Sink::Dir(log), Output::Bucket(k, payload)) => {
                    let soc = soc::sign(
                        &self.signer,
                        bucket_id(self.chain_id, level, index, k),
                        payload,
                    )?;
                    lock(log)?.append_soc(export::SOC, &owner, &soc)?
                }
                (Sink::Bee { bee, batch }, out) => {
                    if set.len() >= limit {
                        join_one(&mut set).await?;
                    }
                    let (bee, batch) = (bee.clone(), *batch);
                    match out {
                        Output::File(reference, file) => set.spawn(async move {
                            let got = bee.upload_file(&batch, file).await?;
                            ensure!(
                                got == reference,
                                "Bee stored a file under {got}, the bucket names {reference}"
                            );
                            Ok(())
                        }),
                        Output::Bucket(k, payload) => {
                            let id = bucket_id(self.chain_id, level, index, k);
                            let soc = soc::sign(&self.signer, id, payload)?;
                            set.spawn(async move {
                                bee.upload_soc(&batch, &owner, &soc).await?;
                                Ok(())
                            })
                        }
                    };
                }
            }
        }
        let built = build.await.context("slice build panicked")??;
        while !set.is_empty() {
            join_one(&mut set).await?;
        }
        // The descriptor last: once it is there, the slice is complete.
        self.upload_soc(
            export::SOC,
            slice_id(self.chain_id, level, index),
            built.record.encode(),
        )
        .await?;
        Ok(Published {
            last_hash,
            last_timestamp,
        })
    }

    /// Uploads one SOC; `kind` says, for an export, whether it is a head ([`export::HEAD`]).
    async fn upload_soc(&self, kind: u8, id: B256, payload: Vec<u8>) -> Result<()> {
        let soc = soc::sign(&self.signer, id, payload)?;
        let owner = self.signer.address();
        match &self.sink {
            Sink::Bee { bee, batch } => {
                bee.upload_soc(batch, &owner, &soc).await?;
            }
            Sink::Dir(log) => lock(log)?.append_soc(kind, &owner, &soc)?,
        }
        Ok(())
    }

    async fn publish_head(&self, n: u64, head: &HeadRecord) -> Result<()> {
        let payload = head.encode();
        self.upload_soc(export::HEAD, head_id(self.chain_id, n), payload.clone())
            .await?;
        if n == 0 {
            self.upload_soc(export::HEAD, origin_id(self.chain_id), payload)
                .await?;
        }
        Ok(())
    }

    /// Saves the state. For an export, the log is synced first and its length recorded in
    /// the state; only after the state is saved may the uploader read that far.
    fn save(&self, state: &mut State, dir: &Path) -> Result<()> {
        match &self.sink {
            Sink::Bee { .. } => state.save(dir),
            Sink::Dir(log) => {
                let mut log = lock(log)?;
                let len = log.checkpoint()?;
                state.export_len = Some(len);
                state.save(dir)?;
                log.commit(len)
            }
        }
    }

    /// Uploads the prepared head of `state`, if any, and records it as published.
    async fn publish_prepared(&self, state: &mut State, dir: &Path) -> Result<()> {
        let Some(head) = state.prepared.clone() else {
            return Ok(());
        };
        failpoint("before-head-upload")?;
        self.publish_head(head.units - state.n0, &head).await?;
        failpoint("after-head-upload")?;
        state.units = head.units;
        state.prepared = None;
        self.save(state, dir)?;
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
        self.publish(slice, before, Some(end)).await?;
        info!(level = slice.0, index = slice.1, "deferred merge uploaded");
        Ok(slice)
    }
}

/// The sort directory of this run, locked for it and emptied of what a stopped run left there
/// (the runs of a slice are removed when it is built, so anything there is from a run that did
/// not finish).
fn sort_dir(opts: &ImportOptions) -> Result<(PathBuf, std::fs::File)> {
    let dir = opts
        .sort_dir
        .clone()
        .unwrap_or_else(|| opts.state_dir.join("sort"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("sort.lock");
    let lock =
        std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    lock.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another addr-import uses the sort directory {} (one importer per sort directory)",
            dir.display()
        )
    })?;
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        let stale = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("slice-"));
        if stale {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok((dir, lock))
}

/// A slice is built only up to `max_level`, measured by what it holds ([`held_level`]): the
/// slice a later start falls into holds less than its grid level. Memory is bounded at any
/// level; the cap bounds the time and the temporary disk one slice takes.
fn check_level(slice: SliceId, first_unit: u64, max_level: u8) -> Result<()> {
    let level = held_level(slice, first_unit);
    ensure!(
        level <= max_level,
        "slice ({}, {}) holds {} units (level {level}), above --max-level {max_level}",
        slice.0,
        slice.1,
        held(slice, first_unit).count()
    );
    Ok(())
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
        // Skip merges a finished one made obsolete.
        let unbuilt = state.schedule.unbuilt();
        let Some(slice) =
            std::iter::from_fn(|| self.queue.pop_front()).find(|s| unbuilt.contains(s))
        else {
            return Ok(());
        };
        let r = held(slice, state.first_unit);
        let end = state.unit_hash(r.end - 1)?;
        let before = state.parent_of(r.start)?;
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

/// Complete units of finalized blocks (the anvil and beacon "finalized" tag).
async fn finalized_units(provider: &impl Provider) -> Result<(u64, u64)> {
    let finalized = provider
        .get_block_by_number(BlockNumberOrTag::Finalized)
        .await
        .context("querying the finalized block")?
        .context("the node reports no finalized block")?
        .header
        .number;
    Ok(((finalized + 1) / UNIT_BLOCKS, finalized))
}

/// How long `--follow` waits between looking for newly finalized units (one slot).
const FOLLOW_POLL: std::time::Duration = std::time::Duration::from_secs(12);

/// What a run did, for the summary line.
pub struct Summary {
    pub owner: Address,
    pub chain_id: u64,
    pub units: u64,
    pub head_index: u64,
}

/// Imports up to the target unit count (or, with `follow`, keeps importing), resuming from the
/// state directory. A run returns only after the merges it started are built.
pub async fn run<P>(
    provider: &P,
    bee: &Bee,
    signer: &PrivateKeySigner,
    opts: ImportOptions,
) -> Result<Summary>
where
    P: Provider + Clone + 'static,
{
    ensure!(
        opts.max_level <= L_MAX && opts.defer_level <= L_MAX,
        "levels above {L_MAX}"
    );
    ensure!(
        !(opts.follow && opts.units.is_some()),
        "--follow imports every finalized unit; it cannot be combined with --units"
    );
    let dir = &opts.state_dir;
    let _lock = StateLock::acquire(dir)?;
    let chain_id = provider.get_chain_id().await.context("querying chain id")?;
    let target_now = |complete: u64, finalized: u64| -> Result<u64> {
        let target = opts.units.unwrap_or(complete);
        ensure!(
            target <= complete,
            "{target} units requested, but only {complete} are finalized (block {finalized})"
        );
        ensure!(
            target <= MAX_UNITS,
            "{target} units exceed the format's {MAX_UNITS}"
        );
        Ok(target)
    };
    let (complete, finalized) = finalized_units(provider).await?;
    let mut target = target_now(complete, finalized)?;
    let roles = role::mask(opts.traces);
    let existing = State::load(dir)?;
    ensure!(
        existing.is_some() || opts.init,
        "no importer state in {}: pass --init to start a new index (only for a key that has \
         never published one), or point --state at the existing state",
        dir.display()
    );
    // Everything that can refuse the run is checked before the sink is opened: opening an
    // export creates it, or truncates it to the state.
    let requested = opts.from_block.map(|b| b / UNIT_BLOCKS);
    let first_unit = match &existing {
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
            ensure!(
                requested.is_none_or(|u| u == s.first_unit),
                "the index in {} starts at block {}; --from-block cannot move it",
                dir.display(),
                s.first_unit * UNIT_BLOCKS
            );
            s.first_unit
        }
        None => {
            let first_unit = requested.unwrap_or(0);
            backfill_slices(target, first_unit, opts.max_level)?;
            first_unit
        }
    };
    let (sort_dir, sort_lock) = sort_dir(&opts)?;
    let sink = match (&opts.out, existing.as_ref().map(|s| s.export_len)) {
        (Some(out), None | Some(Some(_))) => {
            ensure!(
                opts.batch.is_none(),
                "--batch is for uploading; an export takes it at addr-upload"
            );
            let meta = ExportMeta::new(chain_id, signer.address());
            let resume = existing.as_ref().and_then(|s| s.export_len);
            Sink::Dir(std::sync::Arc::new(std::sync::Mutex::new(ExportLog::open(
                out, &meta, resume,
            )?)))
        }
        (None, None | Some(None)) => Sink::Bee {
            bee: bee.clone(),
            batch: match opts.batch {
                Some(b) => b,
                None => bee.usable_batch().await?,
            },
        },
        (Some(_), Some(None)) => bail!(
            "the index in {} uploads to a Bee node; it cannot switch to --out",
            dir.display()
        ),
        (None, Some(Some(_))) => bail!(
            "the index in {} is exported to a directory: pass its --out (and upload it with \
             addr-upload)",
            dir.display()
        ),
    };
    let ctx = Ctx {
        provider: provider.clone(),
        sink,
        signer: signer.clone(),
        chain_id,
        first_unit,
        traces: opts.traces,
        max_level: opts.max_level,
        concurrency: opts.concurrency,
        sort_dir,
        _sort_lock: std::sync::Arc::new(sort_lock),
        // Test hook: a tiny buffer makes even a dev chain's slices take many runs and merge
        // rounds (`scripts/e2e-bee-factory.sh`).
        sort_buffer: std::env::var("ETH2SWARM_ADDR_SORT_BUFFER_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(opts.sort_buffer),
    };

    let mut state = match existing {
        Some(s) => {
            if s.backfilling {
                info!(
                    units = s.n0,
                    first_block = s.first_unit * UNIT_BLOCKS,
                    "resuming the unfinished backfill as it started"
                );
                backfill(&ctx, s.n0, roles, dir).await?
            } else {
                s
            }
        }
        None if matches!(ctx.sink, Sink::Dir(_)) => {
            // Nothing to check against here: addr-upload reads the origin before it sends
            // anything of a new export, and refuses if the key published another index.
            tracing::warn!(
                "exporting a new index for owner {} on chain {chain_id}; addr-upload checks that \
                 this key has not published another one",
                signer.address()
            );
            backfill(&ctx, target, roles, dir).await?
        }
        None => {
            // An origin under this key means an index exists: a second backfill would write
            // another origin and other heads under the same write-once identifiers. The read
            // is best effort: Bee 2.8.3 answers a missing chunk and a failed retrieval alike
            // (500), so only finding the origin is conclusive; --init is the guard. Any SOC of
            // this owner there counts, also one that no longer decodes (an earlier format).
            let address = soc::soc_address(&origin_id(chain_id), &signer.address());
            match bee.get_chunk(&address).await {
                Ok(raw) => {
                    let what = match soc::verify(&raw, &address, &signer.address())
                        .and_then(|p| HeadRecord::decode(&p))
                    {
                        Ok(origin) => format!("origin at {} units", origin.units),
                        Err(e) => format!("an origin this version cannot read: {e:#}"),
                    };
                    bail!(
                        "owner {} already published an index on chain {chain_id} ({what}); \
                         restore its state directory instead of starting over, or use another \
                         key",
                        signer.address()
                    )
                }
                Err(e) => tracing::warn!(
                    "no origin found for owner {} on chain {chain_id}, starting a new index \
                     ({e:#})",
                    signer.address()
                ),
            }
            backfill(&ctx, target, roles, dir).await?
        }
    };
    // A head saved but perhaps not uploaded (the previous run stopped in between).
    ctx.publish_prepared(&mut state, dir).await?;
    info!(
        units = state.units,
        target,
        head = state.head_index(),
        "address index"
    );

    // Merges listed as pending but not built (a previous run stopped): build them again, in
    // the order they were deferred.
    let mut merges = MergeWorker::new(state.schedule.unbuilt());
    merges.kick(&ctx, &state)?;

    loop {
        for n in state.units + 1..=target {
            import_unit(&ctx, &mut state, &mut merges, n, &opts).await?;
        }
        if !opts.follow {
            break;
        }
        merges.collect(&ctx, &mut state, false).await?;
        ctx.save(&mut state, dir)?;
        tokio::time::sleep(FOLLOW_POLL).await;
        let (complete, finalized) = finalized_units(provider).await?;
        target = target_now(complete, finalized)?;
    }

    // Let the merges finish. They stay listed until the next run's first head.
    while merges.len() > 0 {
        merges.collect(&ctx, &mut state, true).await?;
        ctx.save(&mut state, dir)?;
    }
    Ok(Summary {
        owner: signer.address(),
        chain_id,
        units: state.units,
        head_index: state.head_index(),
    })
}

/// Publishes the unit that completes `n` units: its slice (or level-0 slice and a deferred
/// merge), then its head, written ahead.
async fn import_unit<P: Provider + Clone + 'static>(
    ctx: &Ctx<P>,
    state: &mut State,
    merges: &mut MergeWorker,
    n: u64,
    opts: &ImportOptions,
) -> Result<()> {
    merges.collect(ctx, state, false).await?;
    let (now, deferred) = match unit_work(n, opts.defer_level) {
        UnitWork::Build(s) => (s, None),
        UnitWork::Defer { level0, merge } => (level0, Some(merge)),
    };
    // A merge listed as pending must be buildable: fail before a head lists it.
    for s in [Some(now), deferred].into_iter().flatten() {
        check_level(s, state.first_unit, opts.max_level)
            .with_context(|| format!("publishing unit {}", n - 1))?;
    }
    let parent = state.parent_of(held(now, state.first_unit).start)?;
    let f = ctx.publish(now, parent, None).await?;
    state.unit_hashes.insert(n - 1, f.last_hash);
    if let Some(merge) = deferred {
        // Backpressure: not more than MAX_PENDING_MERGES listed at a time.
        while merges.len() >= MAX_PENDING_MERGES {
            merges.collect(ctx, state, true).await?;
        }
        state.schedule.defer(merge);
        merges.queue.push_back(merge);
        merges.kick(ctx, state)?;
    }
    state.prepared = Some(HeadRecord {
        first_unit: state.first_unit,
        units: n,
        last_block_hash: f.last_hash,
        last_block_timestamp: f.last_timestamp,
        pending: state.schedule.next_head_pending(),
    });
    state.prune(n);
    ctx.save(state, &opts.state_dir)?;
    ctx.publish_prepared(state, &opts.state_dir).await
}

/// The slices a backfill of `target` units from `first_unit` writes: what a client reads for
/// head update 0, the cover's slices after the start. Refused if there is nothing to import
/// or a slice is too large to build in memory.
fn backfill_slices(target: u64, first_unit: u64, max_level: u8) -> Result<Vec<SliceId>> {
    ensure!(
        target > first_unit,
        "nothing to import: {target} complete units, the index starts at unit {first_unit} \
         (block {})",
        first_unit * UNIT_BLOCKS
    );
    let slices = resolve_after(first_unit, target, &[]);
    for s in &slices {
        check_level(*s, first_unit, max_level)
            .with_context(|| format!("backfilling {target} units"))?;
    }
    Ok(slices)
}

/// First run: publishes the slices of the cover of `[0, target)` that hold units from the
/// index's start on (every other past slice would never be read), then head update 0 and the
/// origin, written ahead like every head. The start and `target` are saved before the first
/// upload, and a run after a failure resumes with them ([`State::backfilling`]).
async fn backfill<P: Provider + Clone + 'static>(
    ctx: &Ctx<P>,
    target: u64,
    roles: u8,
    dir: &Path,
) -> Result<State> {
    let slices = backfill_slices(target, ctx.first_unit, ctx.max_level)?;
    let mut state = State {
        chain_id: ctx.chain_id,
        owner: ctx.signer.address(),
        roles,
        first_unit: ctx.first_unit,
        n0: target,
        units: target,
        schedule: Schedule::default(),
        unit_hashes: BTreeMap::new(),
        prepared: None,
        export_len: None,
        backfilling: true,
    };
    ctx.save(&mut state, dir)?;
    let (mut parent, mut last) = (None, None);
    for s in slices {
        info!(level = s.0, index = s.1, "backfill");
        let f = ctx.publish(s, parent, None).await?;
        failpoint("backfill-slice")?;
        parent = Some(f.last_hash);
        state.unit_hashes.insert(units(s).end - 1, f.last_hash);
        last = Some(f);
    }
    let last = last.expect("a cover of at least one unit");
    state.prepared = Some(HeadRecord {
        first_unit: ctx.first_unit,
        units: target,
        last_block_hash: last.last_hash,
        last_block_timestamp: last.last_timestamp,
        pending: Vec::new(),
    });
    state.backfilling = false;
    // From here on the origin's N0 is fixed: a later run publishes this head, never another.
    ctx.save(&mut state, dir)?;
    info!(
        units = target,
        first_block = ctx.first_unit * UNIT_BLOCKS,
        "backfill uploaded; publishing head 0 and the origin"
    );
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    /// pending list must encode, and every merge must eventually leave the heads. Also for an
    /// index that starts after genesis, whose backfill writes only the slices after its start.
    #[test]
    fn every_head_references_written_slices() {
        for (first_unit, defer_level, delay) in [0u64, 2, 5, 37]
            .into_iter()
            .flat_map(|f| [1u8, 2, 3, 5].map(move |d| (f, d)))
            .flat_map(|(f, d)| [0u64, 1, 2, 5, 17].map(move |x| (f, d, x)))
        {
            let n0 = first_unit + 3;
            let mut written: BTreeSet<SliceId> =
                resolve_after(first_unit, n0, &[]).into_iter().collect();
            let mut schedule = Schedule::default();
            // (finishes after unit, slice)
            let mut running: Vec<(u64, SliceId)> = Vec::new();
            for n in n0 + 1..=first_unit + 300 {
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
                    first_unit,
                    units: n,
                    last_block_hash: B256::ZERO,
                    last_block_timestamp: 0,
                    pending: pending.clone(),
                };
                assert_eq!(HeadRecord::decode(&head.encode()).unwrap(), head);
                for s in resolve_after(first_unit, n, &pending) {
                    assert!(
                        written.contains(&s),
                        "start {first_unit}, defer {defer_level}, delay {delay}: head {n} \
                             reads {s:?}, not written (pending {pending:?})"
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

    #[test]
    fn state_roundtrip_and_pruning() {
        let dir = std::env::temp_dir().join(format!("addr-import-test-{}", std::process::id()));
        let mut s = State {
            chain_id: 1337,
            owner: Address::repeat_byte(1),
            roles: 0xdf,
            first_unit: 0,
            n0: 5,
            units: 9,
            schedule: Schedule {
                pending: vec![(1, 4)],
                built: vec![],
            },
            unit_hashes: BTreeMap::new(),
            prepared: Some(HeadRecord {
                first_unit: 0,
                units: 10,
                last_block_hash: B256::repeat_byte(2),
                last_block_timestamp: 1_800_000_000,
                pending: vec![(1, 4)],
            }),
            export_len: None,
            backfilling: false,
        };
        s.save(&dir).unwrap();
        assert_eq!(State::load(&dir).unwrap(), Some(s.clone()));
        assert_eq!(s.head_index(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(State::load(&dir).unwrap(), None);

        // After 13 units (cover (3, 0), (2, 2), (0, 12)), only the cover's last units are
        // kept: 7, 11 and 12. Where the next slices start, they continue from these.
        s.schedule = Schedule::default();
        for u in 0..13 {
            s.unit_hashes.insert(u, B256::repeat_byte(u as u8));
        }
        s.prune(13);
        assert_eq!(
            s.unit_hashes.keys().copied().collect::<Vec<_>>(),
            vec![7, 11, 12]
        );
        // Every slice the importer builds next starts right after a kept unit.
        for n in 14..40 {
            let start = units(new_slice(n)).start;
            assert!(
                start == 0 || s.unit_hashes.contains_key(&(start - 1)),
                "unit {}: no hash for unit {}",
                n - 1,
                start - 1
            );
            s.unit_hashes.insert(n - 1, B256::ZERO);
            s.prune(n);
        }

        // A top-level merge still pending after its boundary keeps both of its ends.
        let top = 1u64 << L_MAX;
        let mut s2 = s.clone();
        s2.unit_hashes = (top - 2..2 * top + 100).map(|u| (u, B256::ZERO)).collect();
        s2.schedule.defer((L_MAX, 1)); // units top..2·top, due at unit 2·top − 1
        s2.prune(2 * top + 100);
        assert!(s2.unit_hash(top - 1).is_ok(), "the unit before the merge");
        assert!(s2.unit_hash(2 * top - 1).is_ok(), "the merge's last unit");
        assert!(s2.unit_hash(top - 2).is_err());

        // An index from unit 5: a slice that starts with the index continues nothing; every
        // other slice continues a kept unit.
        let mut s3 = State {
            first_unit: 5,
            schedule: Schedule::default(),
            unit_hashes: (5..13).map(|u| (u, B256::ZERO)).collect(),
            ..s.clone()
        };
        assert_eq!(s3.parent_of(5).unwrap(), None);
        s3.prune(13);
        for n in 14..80 {
            let start = held(new_slice(n), 5).start;
            assert!(s3.parent_of(start).is_ok(), "unit {}: no parent", n - 1);
            s3.unit_hashes.insert(n - 1, B256::ZERO);
            s3.prune(n);
        }
        // A pending merge the start falls into keeps its last unit, and nothing before it.
        s3.schedule.defer((7, 0)); // units 0..128, held from 5
        s3.unit_hashes = (4..130).map(|u| (u, B256::ZERO)).collect();
        s3.prune(130);
        assert!(s3.unit_hash(127).is_ok());
        assert!(s3.unit_hash(4).is_err());
    }
}
