//! Exporting the address index to a directory instead of a Bee node, and `addr-upload`: pushing
//! an export through any Bee node later (`docs/address-index.md`, "Exporting for a later
//! upload").
//!
//! An export is one append-only log of records in upload order, `kind (u8) ‖ key (32) ‖
//! life (u64 BE) ‖ len (u32 BE) ‖ data` (layouts 2 and 3; layout 1 has no `life`, and only
//! layout 3 has [`CHUNK`] records, so that a build that cannot upload them refuses the export
//! up front):
//!
//! * [`FILE`]: a spill or pages file; the key is its Swarm reference.
//! * [`SOC`]: a slice descriptor or a bucket, as a SOC in wire form
//!   (`identifier ‖ signature ‖ span ‖ payload`); the key is its address.
//! * [`HEAD`]: a head update or the origin, the same way. A head is a barrier: everything
//!   before it is uploaded before it, as the importer does with a Bee node.
//! * [`CHUNK`]: one chunk of a spill file too large to be written whole, in wire form, under
//!   its address; the file's root chunk comes after the others (layout 3).
//!
//! `life` is how many more units the record must stay retrievable after it was written
//! (`u64::MAX`: permanently), which `addr-upload` matches with a batch's TTL
//! ([`super::stamps`]). A layout-1 log, written before lifetimes, is read as all permanent.
//!
//! The importer writes records as it would upload them and checkpoints the log at every save
//! of its state: the log is synced, its length goes into the state, and only then into the
//! export's `committed` file. A run that resumes truncates the log to the length in its state
//! (the records of an unfinished unit are written again, identically), and the uploader reads
//! no further than `committed`, which is never beyond that length, so a truncation never takes
//! away a record it may have uploaded.
//!
//! A slice is written record by record as it is built, so a deferred merge building in the
//! background interleaves with the units, their heads and the checkpoints. Its descriptor is
//! still in the log before the head that drops it from `pending`, and no head makes a client
//! read a merge before that; but `committed` can fall inside a merge, and a run that stops
//! while one is written keeps the part before its last save and writes the whole merge again
//! after it. The copies are identical; they cost disk and a second upload.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::io::{join_one, write_atomic};
use super::layout::origin_id;
use super::stamps::Stamps;
use crate::swarm::bee::Bee;
use crate::swarm::file::file_reference;
use crate::swarm::soc::{self, SignedSoc, soc_address};

pub const FILE: u8 = 1;
pub const SOC: u8 = 2;
pub const HEAD: u8 = 3;
/// One chunk of a large spill file, under its address, in wire form (`span ‖ payload`):
/// such a file is never held whole (layout 3 only).
pub const CHUNK: u8 = 4;

/// Bytes before a record's data, in layout `format`.
fn record_header(format: u32) -> u64 {
    if format >= 2 {
        1 + 32 + 8 + 4
    } else {
        1 + 32 + 4
    }
}

/// `life` of a permanent record.
const PERMANENT: u64 = u64::MAX;

const META_FILE: &str = "addr1-export.json";
const LOG_FILE: &str = "addr1.log";
const COMMITTED_FILE: &str = "committed";
const PROGRESS_FILE: &str = "uploaded.json";
/// Held by the one importer writing the log.
const WRITER_LOCK: &str = "addr1.log.lock";
/// Held by the one `addr-upload` of the directory.
const UPLOAD_LOCK: &str = "upload.lock";
/// The importer's state lock (`import::StateLock`), there if `--out` is the state directory.
const STATE_LOCK: &str = "addr-import.lock";
/// The importer's own directory for sort runs, inside `--sort-dir` (default: the state
/// directory), so there too if `--out` is the state directory.
pub const SORT_DIR: &str = "ethswarm-sort";
/// The version of the export's layout, in its metadata: the log is a contract between two
/// commands that may be different builds on different machines.
pub const FORMAT: u32 = 3;
/// Layouts this build reads (and goes on writing, for an export that started in one).
const FORMATS: [u32; 3] = [1, 2, 3];
/// `addr-upload` saves its progress at least this often within a stretch without a head (a
/// backfill, a large merge), so that a failed record does not send it back to the last head.
const SAVE_EVERY: u64 = 2048;

/// An exclusive lock on `dir/name`, held while the value lives.
struct DirLock(#[allow(dead_code)] File);

impl DirLock {
    fn acquire(dir: &Path, name: &str, who: &str) -> Result<Self> {
        let path = dir.join(name);
        let f = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        f.try_lock()
            .map_err(|_| anyhow::anyhow!("another {who} holds {}", path.display()))?;
        Ok(Self(f))
    }
}

/// What an export is of: one owner's index of one chain, in layout [`FORMAT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportMeta {
    pub format: u32,
    pub chain_id: u64,
    pub owner: Address,
}

impl ExportMeta {
    pub fn new(chain_id: u64, owner: Address) -> Self {
        Self {
            format: FORMAT,
            chain_id,
            owner,
        }
    }

    fn read(dir: &Path) -> Result<Self> {
        let path = dir.join(META_FILE);
        let meta: Self = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("{} holds no export", dir.display()))?,
        )
        .with_context(|| format!("reading {}", path.display()))?;
        ensure!(
            FORMATS.contains(&meta.format),
            "{} is an export in layout {}, this build reads layouts {FORMATS:?}",
            dir.display(),
            meta.format
        );
        Ok(meta)
    }
}

/// The committed length of the export in `dir`, if it has one.
fn read_committed(dir: &Path) -> Result<Option<u64>> {
    let path = dir.join(COMMITTED_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(
            s.trim()
                .parse()
                .with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The log the importer appends to.
pub struct ExportLog {
    dir: PathBuf,
    /// The layout written: the export's own (an export started in layout 1 stays in it).
    format: u32,
    out: BufWriter<File>,
    len: u64,
    /// An append failed part way: the file may hold bytes `len` does not count, so nothing
    /// more may be checkpointed. The next run truncates them away.
    broken: bool,
    _lock: DirLock,
}

impl ExportLog {
    /// Opens the export in `dir` for `meta`. `resume_at` is the log length the importer's
    /// state recorded: the log is truncated to it (records after it belong to a unit that
    /// is written again). `None` starts a new export, in a directory without one. A resumed
    /// export keeps its layout.
    pub fn open(dir: &Path, meta: &ExportMeta, resume_at: Option<u64>) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = DirLock::acquire(dir, WRITER_LOCK, "addr-import")?;
        let meta_path = dir.join(META_FILE);
        let log_path = dir.join(LOG_FILE);
        let format = match resume_at {
            None => {
                ensure!(
                    is_fresh(dir, meta)?,
                    "{} is not empty; a new export needs an empty directory",
                    dir.display()
                );
                write_atomic(&meta_path, &serde_json::to_vec_pretty(meta)?)?;
                meta.format
            }
            Some(_) => {
                let found = ExportMeta::read(dir)?;
                ensure!(
                    (found.chain_id, found.owner) == (meta.chain_id, meta.owner),
                    "{} is an export for chain {}, owner {}; this run is for chain {}, owner {}",
                    dir.display(),
                    found.chain_id,
                    found.owner,
                    meta.chain_id,
                    meta.owner
                );
                found.format
            }
        };
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;
        let mut len = file.metadata()?.len();
        let at = resume_at.unwrap_or(0);
        ensure!(
            len >= at,
            "{} has {len} bytes, the importer's state recorded {at}: the export lost records",
            log_path.display()
        );
        // The uploader may have sent everything up to `committed`: a state that recorded less
        // is older than the export (a restored backup) and must not truncate below it.
        if let Some(committed) = read_committed(dir)? {
            ensure!(
                committed <= at,
                "{} is committed up to {committed}, the importer's state recorded {at}: the \
                 state is older than the export",
                log_path.display()
            );
        }
        if len > at {
            info!(
                from = len,
                to = at,
                "truncating the export to the importer's state"
            );
            file.set_len(at)?;
            file.sync_all()?;
            len = at;
        }
        let log = Self {
            dir: dir.to_path_buf(),
            format,
            out: BufWriter::with_capacity(1 << 20, file),
            len,
            broken: false,
            _lock: lock,
        };
        // A run that stopped before writing `committed` leaves it behind the log.
        log.commit(len)?;
        Ok(log)
    }

    /// Appends a record that must live `life` more units (`None`: permanently).
    pub fn append(&mut self, kind: u8, key: &B256, life: Option<u64>, data: &[u8]) -> Result<()> {
        ensure!(!self.broken, "an earlier write to the export failed");
        let n = u32::try_from(data.len()).context("record over 4 GiB")?;
        self.broken = true;
        self.out.write_all(&[kind])?;
        self.out.write_all(key.as_slice())?;
        if self.format >= 2 {
            let life = life.map_or(PERMANENT, |l| l.min(PERMANENT - 1));
            self.out.write_all(&life.to_be_bytes())?;
        }
        self.out.write_all(&n.to_be_bytes())?;
        self.out.write_all(data)?;
        self.broken = false;
        self.len += record_header(self.format) + data.len() as u64;
        Ok(())
    }

    /// The layout this log is written in.
    pub fn format(&self) -> u32 {
        self.format
    }

    pub fn append_soc(
        &mut self,
        kind: u8,
        owner: &Address,
        life: Option<u64>,
        soc: &SignedSoc,
    ) -> Result<()> {
        self.append(
            kind,
            &soc_address(&soc.identifier, owner),
            life,
            &soc.wire(),
        )
    }

    /// Writes everything out and syncs it: the length to record in the importer's state.
    pub fn checkpoint(&mut self) -> Result<u64> {
        ensure!(!self.broken, "an earlier write to the export failed");
        self.out.flush()?;
        self.out.get_ref().sync_data()?;
        Ok(self.len)
    }

    /// Lets the uploader read up to `len` (after the state recording it was saved).
    pub fn commit(&self, len: u64) -> Result<()> {
        write_atomic(&self.dir.join(COMMITTED_FILE), len.to_string().as_bytes())
    }
}

/// A directory a new export may start in: empty, or holding only what an earlier attempt for
/// the same index left before it wrote any record (its metadata, an empty log, the locks).
fn is_fresh(dir: &Path, meta: &ExportMeta) -> Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let ok = match name.to_str() {
            Some(WRITER_LOCK | UPLOAD_LOCK | STATE_LOCK) => true,
            Some(SORT_DIR) => entry.file_type()?.is_dir(),
            Some(COMMITTED_FILE) => read_committed(dir)? == Some(0),
            Some(LOG_FILE) => entry.metadata()?.len() == 0,
            Some(META_FILE) => {
                serde_json::from_slice::<ExportMeta>(&std::fs::read(entry.path())?).ok()
                    == Some(meta.clone())
            }
            _ => false,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// One record of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Offset of the record in the log.
    pub offset: u64,
    pub kind: u8,
    pub key: B256,
    /// Units it must still live when written (`None`: permanently, or a layout-1 log).
    pub life: Option<u64>,
    pub data: Vec<u8>,
    /// Offset of the next record.
    end: u64,
}

impl Record {
    fn end(&self) -> u64 {
        self.end
    }
}

/// Reads the records of the log in `dir` from `offset` up to `end`.
pub struct Records {
    reader: BufReader<File>,
    format: u32,
    offset: u64,
    end: u64,
}

impl Records {
    /// `end` is the committed length: the log must hold at least that much (a copy of a live
    /// export can have its `committed` ahead of its log).
    pub fn open(dir: &Path, format: u32, offset: u64, end: u64) -> Result<Self> {
        let path = dir.join(LOG_FILE);
        let mut f = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let len = f.metadata()?.len();
        ensure!(
            end <= len,
            "{} has {len} bytes, `committed` says {end}: an incomplete copy? Copy `committed` \
             first and the log after it, or stop the importer while copying",
            path.display()
        );
        ensure!(
            offset <= end,
            "reading {} from {offset}, past its committed length {end}",
            path.display()
        );
        f.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            reader: BufReader::with_capacity(1 << 20, f),
            format,
            offset,
            end,
        })
    }

    pub fn next_record(&mut self) -> Result<Option<Record>> {
        if self.offset >= self.end {
            return Ok(None);
        }
        let header = record_header(self.format);
        let mut head = vec![0u8; header as usize];
        self.reader
            .read_exact(&mut head)
            .with_context(|| format!("reading the record at {}", self.offset))?;
        let kind = head[0];
        ensure!(
            matches!(kind, FILE | SOC | HEAD) || (kind == CHUNK && self.format >= 3),
            "record of unknown kind {kind} at {}",
            self.offset
        );
        let key = B256::from_slice(&head[1..33]);
        let life = (self.format >= 2)
            .then(|| u64::from_be_bytes(head[33..41].try_into().unwrap()))
            .filter(|l| *l != PERMANENT);
        let n = u32::from_be_bytes(head[head.len() - 4..].try_into().unwrap()) as usize;
        // Checked before allocating: a damaged length must not ask for gigabytes.
        ensure!(
            self.offset + header + n as u64 <= self.end,
            "the record at {} runs past the committed length {}",
            self.offset,
            self.end
        );
        let mut data = vec![0u8; n];
        self.reader.read_exact(&mut data)?;
        let rec = Record {
            offset: self.offset,
            kind,
            key,
            life,
            end: self.offset + header + n as u64,
            data,
        };
        self.offset = rec.end();
        Ok(Some(rec))
    }
}

/// How far `addr-upload` got: the log offset up to which every record is uploaded, and the
/// last head among them with its offset, which a resumed upload checks against the log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub offset: u64,
    pub last_head: Option<B256>,
    /// Offset of that head's record.
    pub last_head_at: u64,
    /// The batch the records were stamped with: an upload with another batch (another node,
    /// or a new batch after the old one expired) starts from the beginning.
    pub batch: Option<B256>,
}

impl Progress {
    fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// What an upload did.
#[derive(Debug, Default)]
pub struct UploadSummary {
    /// Records (heads included) per batch.
    pub batches: std::collections::BTreeMap<B256, u64>,
    /// Records of lower slices that went to the permanent batch: no lower batch lived long
    /// enough.
    pub fell_back: u64,
    pub owner: Address,
    pub chain_id: u64,
    pub files: u64,
    /// Chunks of large spill files.
    pub chunks: u64,
    pub socs: u64,
    pub heads: u64,
    /// The log offset uploaded to, of `committed`.
    pub offset: u64,
    pub committed: u64,
}

/// Checks a record before it is uploaded: a file against its reference, a SOC against its
/// address and the export's owner. Returns the SOC for SOC records.
fn check(rec: &Record, owner: &Address) -> Result<Option<SignedSoc>> {
    match rec.kind {
        CHUNK => {
            crate::swarm::file::checked_chunk(&rec.data, &rec.key, None)
                .with_context(|| format!("chunk record at {}", rec.offset))?;
            Ok(None)
        }
        FILE => {
            let got = file_reference(&rec.data);
            ensure!(
                got == rec.key,
                "file at {} hashes to {got}, the log names {}",
                rec.offset,
                rec.key
            );
            Ok(None)
        }
        _ => {
            let parsed = soc::parse(&rec.data)?;
            ensure!(
                parsed.signer == Some(*owner),
                "SOC at {} is not signed by the export's owner {owner}",
                rec.offset
            );
            let address = soc_address(&parsed.identifier, owner);
            ensure!(
                address == rec.key,
                "SOC at {} has address {address}, the log names {}",
                rec.offset,
                rec.key
            );
            Ok(Some(SignedSoc {
                identifier: parsed.identifier,
                signature: parsed.signature,
                payload: parsed.payload,
            }))
        }
    }
}

/// Refuses an export whose key already published another index: if Bee has a chunk at the
/// origin's address, it must be this export's origin (a repeated upload). Bee 2.8.3 answers a
/// missing and an unretrievable chunk alike, so only a chunk that comes back is conclusive.
async fn check_origin(bee: &Bee, dir: &Path, meta: &ExportMeta, committed: u64) -> Result<()> {
    let address = soc_address(&origin_id(meta.chain_id), &meta.owner);
    let published = match bee.clone().with_retries(2).get_chunk(&address).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(
                "no origin found for owner {} on chain {}: uploading a new index ({e:#})",
                meta.owner,
                meta.chain_id
            );
            return Ok(());
        }
    };
    let ours = find_record(dir, meta.format, committed, HEAD, &address)?;
    ensure!(
        ours.as_deref() == Some(&published[..]),
        "owner {} already published an index on chain {} with another origin: this export \
         would write other records under the same identifiers. Export under another key.",
        meta.owner,
        meta.chain_id
    );
    Ok(())
}

/// The data of the first record of `kind` with `key` in the log up to `end`, reading only the
/// record headers of the others.
fn find_record(dir: &Path, format: u32, end: u64, kind: u8, key: &B256) -> Result<Option<Vec<u8>>> {
    let path = dir.join(LOG_FILE);
    let mut f = BufReader::new(File::open(&path)?);
    let mut offset = 0u64;
    let header = record_header(format);
    while offset < end {
        let mut head = vec![0u8; header as usize];
        f.read_exact(&mut head)?;
        let n = u32::from_be_bytes(head[head.len() - 4..].try_into().unwrap()) as u64;
        ensure!(
            offset + header + n <= end,
            "the record at {offset} runs past the committed length {end}"
        );
        if head[0] == kind && &head[1..33] == key.as_slice() {
            let mut data = vec![0u8; n as usize];
            f.read_exact(&mut data)?;
            return Ok(Some(data));
        }
        f.seek_relative(n as i64)?;
        offset += header + n;
    }
    Ok(None)
}

async fn upload_one(
    bee: Bee,
    batch: B256,
    owner: Address,
    rec: Record,
    soc: Option<SignedSoc>,
) -> Result<()> {
    let got = match soc {
        None if rec.kind == CHUNK => bee.upload_chunk(&batch, rec.data).await?,
        None => bee.upload_file(&batch, rec.data).await?,
        Some(soc) => bee.upload_soc(&batch, &owner, &soc).await?,
    };
    ensure!(
        got == rec.key,
        "Bee stored the record at {} under {got}, the log names {}",
        rec.offset,
        rec.key
    );
    Ok(())
}

/// How `addr-upload` stamps: the permanent batch (default: the one the earlier uploads of the
/// directory used, else the node's first usable batch), the batches for lower slices, and the
/// grace period they must outlive a slice by.
#[derive(Debug, Clone, Default)]
pub struct Batches {
    pub permanent: Option<B256>,
    pub lower: Vec<B256>,
    pub grace_days: u64,
    /// Seconds per block of the chain.
    pub slot_secs: u64,
}

/// Uploads the export in `dir` through `bee`, from where the last upload of this directory
/// stopped (or from the start), up to its committed length. Records between heads go up in
/// parallel; each head waits for everything before it, then the progress is saved. Heads and
/// permanent records go to the permanent batch, a lower slice's records to the lower batch
/// that expires soonest after the slice ([`Stamps`]).
pub async fn upload(
    bee: &Bee,
    batches: Batches,
    dir: &Path,
    concurrency: usize,
    from_start: bool,
) -> Result<UploadSummary> {
    let meta = ExportMeta::read(dir)?;
    let _lock = DirLock::acquire(dir, UPLOAD_LOCK, "addr-upload")?;
    let committed = read_committed(dir)?.context("the export has no committed length")?;
    let progress_path = dir.join(PROGRESS_FILE);
    let mut progress: Progress = match (from_start, std::fs::read(&progress_path)) {
        (false, Ok(bytes)) => serde_json::from_slice(&bytes)?,
        (false, Err(e)) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("reading {}", progress_path.display()));
        }
        _ => Progress::default(),
    };
    let batch = match (batches.permanent, progress.batch) {
        (Some(b), _) | (None, Some(b)) => b,
        (None, None) => {
            // With lower batches, the node's "first usable batch" may well be a short one:
            // the permanent records must not land there by accident.
            ensure!(
                batches.lower.is_empty(),
                "--lower-batch needs the permanent batch named with --batch"
            );
            bee.usable_batch().await?
        }
    };
    if let Some(recorded) = progress.batch {
        ensure!(
            recorded == batch,
            "{} was uploaded with batch {recorded} so far: pass --batch {recorded} to continue, \
             or --from-start to upload everything again with batch {batch}",
            dir.display()
        );
    }
    progress.batch = Some(batch);
    if meta.format < 2 && !batches.lower.is_empty() {
        tracing::warn!(
            "{} is a layout-{} export, written before lifetimes: every record is permanent and \
             goes to {batch}; --lower-batch applies only to an export started by this build",
            dir.display(),
            meta.format
        );
    }
    let stamps = std::sync::Arc::new(
        Stamps::open(
            bee.clone(),
            batch,
            batches.lower,
            batches.grace_days,
            batches.slot_secs,
        )
        .await?,
    );
    // The log must still hold the head the last upload ended with.
    if let Some(head) = progress.last_head {
        let mut r = Records::open(dir, meta.format, progress.last_head_at, committed)?;
        let rec = r.next_record()?;
        match rec {
            Some(rec) if rec.kind == HEAD && rec.key == head && rec.end() <= progress.offset => {}
            _ => bail!(
                "the export no longer matches the last upload (head {head} at {}); start over \
                 with --from-start (uploads are idempotent)",
                progress.last_head_at
            ),
        }
    }
    // Before a fresh upload sends anything: the key must not have published another index.
    if progress.last_head.is_none() {
        check_origin(bee, dir, &meta, committed).await?;
    }
    let mut summary = UploadSummary {
        owner: meta.owner,
        chain_id: meta.chain_id,
        offset: progress.offset,
        committed,
        ..Default::default()
    };
    let mut records = Records::open(dir, meta.format, progress.offset, committed)?;
    let mut set = tokio::task::JoinSet::new();
    let limit = concurrency.max(1);
    let mut since_save = 0u64;
    while let Some(rec) = records.next_record()? {
        let soc = check(&rec, &meta.owner)?;
        let (kind, key, offset, end) = (rec.kind, rec.key, rec.offset, rec.end());
        if kind == HEAD {
            while !set.is_empty() {
                join_one(&mut set).await?;
            }
            let b = stamps.batch_for(None).await?;
            upload_one(bee.clone(), b, meta.owner, rec, soc).await?;
            summary.heads += 1;
            progress = Progress {
                offset: end,
                last_head: Some(key),
                last_head_at: offset,
                batch: Some(batch),
            };
            progress.save(&progress_path)?;
            since_save = 0;
        } else {
            if set.len() >= limit {
                join_one(&mut set).await?;
            }
            match kind {
                FILE => summary.files += 1,
                CHUNK => summary.chunks += 1,
                _ => summary.socs += 1,
            }
            let b = stamps.batch_for(rec.life).await?;
            set.spawn(upload_one(bee.clone(), b, meta.owner, rec, soc));
        }
        summary.offset = end;
        since_save += 1;
        // Within a long stretch without a head: wait for what is in flight, then save the
        // offset (the last head stays what a resumed upload checks).
        if since_save >= SAVE_EVERY {
            while !set.is_empty() {
                join_one(&mut set).await?;
            }
            progress.offset = summary.offset;
            progress.save(&progress_path)?;
            since_save = 0;
        }
    }
    while !set.is_empty() {
        join_one(&mut set).await?;
    }
    // Records after the last head (a merge built after it) are uploaded too.
    if summary.offset > progress.offset {
        progress.offset = summary.offset;
        progress.save(&progress_path)?;
    }
    summary.batches = stamps.used();
    summary.fell_back = stamps.fell_back();
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::signers::local::PrivateKeySigner;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("addr-export-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn read_all(dir: &Path, end: u64) -> Vec<Record> {
        let format = ExportMeta::read(dir).unwrap().format;
        let mut r = Records::open(dir, format, 0, end).unwrap();
        let mut out = Vec::new();
        while let Some(rec) = r.next_record().unwrap() {
            out.push(rec);
        }
        out
    }

    #[test]
    fn lifetimes_roundtrip_and_layout_1_reads_as_permanent() {
        let signer = PrivateKeySigner::random();
        let file = vec![5u8; 100];
        let reference = file_reference(&file);
        let head = soc::sign(&signer, B256::repeat_byte(2), b"head".to_vec()).unwrap();
        for format in [1, 2] {
            let dir = tmp(&format!("layout{format}"));
            let meta = ExportMeta {
                format,
                ..ExportMeta::new(1, signer.address())
            };
            let mut log = ExportLog::open(&dir, &meta, None).unwrap();
            log.append(FILE, &reference, Some(12), &file).unwrap();
            log.append_soc(HEAD, &meta.owner, None, &head).unwrap();
            let end = log.checkpoint().unwrap();
            log.commit(end).unwrap();
            drop(log);
            assert_eq!(
                end,
                2 * record_header(format) + 100 + head.wire().len() as u64
            );
            let recs = read_all(&dir, end);
            let lives: Vec<_> = recs.iter().map(|r| r.life).collect();
            // Layout 1 has no lifetimes: everything is permanent.
            let want = if format == 1 {
                [None, None]
            } else {
                [Some(12), None]
            };
            assert_eq!(lives, want, "layout {format}");
            assert_eq!(recs[0].data, file);
            // A resumed export keeps its layout, whatever this build writes.
            let log =
                ExportLog::open(&dir, &ExportMeta::new(1, signer.address()), Some(end)).unwrap();
            assert_eq!(log.format, format);
            drop(log);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn log_roundtrip_truncation_and_checks() {
        let dir = tmp("log");
        let signer = PrivateKeySigner::random();
        let meta = ExportMeta::new(1, signer.address());
        let file = vec![7u8; 5000];
        let reference = file_reference(&file);
        let soc = soc::sign(&signer, B256::repeat_byte(1), b"bucket".to_vec()).unwrap();
        let head = soc::sign(&signer, B256::repeat_byte(2), b"head".to_vec()).unwrap();

        let mut log = ExportLog::open(&dir, &meta, None).unwrap();
        log.append(FILE, &reference, Some(3), &file).unwrap();
        log.append_soc(SOC, &meta.owner, Some(3), &soc).unwrap();
        let at_unit = log.checkpoint().unwrap();
        log.append_soc(HEAD, &meta.owner, None, &head).unwrap();
        let end = log.checkpoint().unwrap();
        log.commit(end).unwrap();
        drop(log);

        let recs = read_all(&dir, end);
        assert_eq!(
            recs.iter().map(|r| r.kind).collect::<Vec<_>>(),
            [FILE, SOC, HEAD]
        );
        assert!(check(&recs[0], &meta.owner).unwrap().is_none());
        let back = check(&recs[1], &meta.owner).unwrap().unwrap();
        assert_eq!(back.wire(), soc.wire());
        // Another owner, a changed file, a wrong key: refused.
        assert!(check(&recs[1], &Address::repeat_byte(9)).is_err());
        let mut bad = recs[0].clone();
        bad.data[0] ^= 1;
        assert!(check(&bad, &meta.owner).is_err());
        let mut bad = recs[1].clone();
        bad.key = B256::ZERO;
        assert!(check(&bad, &meta.owner).is_err());

        // A new export refuses a directory that has one; another owner's run refuses it too.
        assert!(ExportLog::open(&dir, &meta, None).is_err());
        let other = ExportMeta {
            chain_id: 2,
            ..meta.clone()
        };
        assert!(ExportLog::open(&dir, &other, Some(end)).is_err());
        // A state further than the log: records were lost.
        assert!(ExportLog::open(&dir, &meta, Some(end + 1)).is_err());
        // A state older than what was committed (the uploader may have sent it) is refused.
        assert!(ExportLog::open(&dir, &meta, Some(at_unit)).is_err());
        // The importer's own case: the state recorded `at_unit`, the run stopped before
        // committing more, and the log holds records after it. They are dropped.
        std::fs::write(dir.join(COMMITTED_FILE), at_unit.to_string()).unwrap();
        let log = ExportLog::open(&dir, &meta, Some(at_unit)).unwrap();
        assert_eq!(log.len, at_unit);
        drop(log);
        assert_eq!(read_all(&dir, at_unit).len(), 2);
        assert_eq!(
            std::fs::read_to_string(dir.join(COMMITTED_FILE)).unwrap(),
            at_unit.to_string()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_export_needs_a_fresh_directory() {
        let dir = tmp("fresh");
        let meta = ExportMeta::new(1, Address::repeat_byte(1));
        // An attempt that stopped before writing a record can be repeated.
        drop(ExportLog::open(&dir, &meta, None).unwrap());
        drop(ExportLog::open(&dir, &meta, None).unwrap());
        // Not for another owner, and not with another upload's progress left behind.
        let other = ExportMeta {
            owner: Address::repeat_byte(2),
            ..meta.clone()
        };
        assert!(ExportLog::open(&dir, &other, None).is_err());
        std::fs::write(dir.join(PROGRESS_FILE), "{}").unwrap();
        assert!(ExportLog::open(&dir, &meta, None).is_err());
        std::fs::remove_file(dir.join(PROGRESS_FILE)).unwrap();
        // One writer at a time.
        let log = ExportLog::open(&dir, &meta, None).unwrap();
        assert!(ExportLog::open(&dir, &meta, Some(0)).is_err());
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_length_is_refused_before_it_is_read() {
        let dir = tmp("framing");
        std::fs::create_dir_all(&dir).unwrap();
        let mut rec = vec![FILE];
        rec.extend_from_slice(&[0u8; 32]);
        rec.extend_from_slice(&PERMANENT.to_be_bytes());
        rec.extend_from_slice(&u32::MAX.to_be_bytes());
        rec.extend_from_slice(&[0u8; 10]);
        std::fs::write(dir.join(LOG_FILE), &rec).unwrap();
        let err = Records::open(&dir, FORMAT, 0, rec.len() as u64)
            .unwrap()
            .next_record()
            .unwrap_err();
        assert!(format!("{err:#}").contains("runs past"), "{err:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
