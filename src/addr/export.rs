//! Exporting the address index to a directory instead of a Bee node, and `addr-upload`: pushing
//! an export through any Bee node later (`docs/address-index.md`, "Exporting for a later
//! upload").
//!
//! An export is one append-only log of records in upload order, `kind (u8) ‖ key (32) ‖
//! len (u32 BE) ‖ data`:
//!
//! * [`FILE`]: a spill or pages file; the key is its Swarm reference.
//! * [`SOC`]: a slice descriptor or a bucket, as a SOC in wire form
//!   (`identifier ‖ signature ‖ span ‖ payload`); the key is its address.
//! * [`HEAD`]: a head update or the origin, the same way. A head is a barrier: everything
//!   before it is uploaded before it, as the importer does with a Bee node.
//!
//! The importer writes records as it would upload them and checkpoints the log at every save
//! of its state: the log is synced, its length goes into the state, and only then into the
//! export's `committed` file. A run that resumes truncates the log to the length in its state
//! (the records of an unfinished unit are written again, identically), and the uploader reads
//! no further than `committed`, which is never beyond that length, so a truncation never takes
//! away a record it may have uploaded.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::swarm::bee::Bee;
use crate::swarm::file::file_reference;
use crate::swarm::soc::{self, SignedSoc, soc_address};

pub const FILE: u8 = 1;
pub const SOC: u8 = 2;
pub const HEAD: u8 = 3;

/// Bytes before a record's data.
const RECORD_HEADER: u64 = 1 + 32 + 4;

const META_FILE: &str = "addr1-export.json";
const LOG_FILE: &str = "addr1.log";
const COMMITTED_FILE: &str = "committed";
const PROGRESS_FILE: &str = "uploaded.json";
/// Held by the one importer writing the log.
const WRITER_LOCK: &str = "addr1.log.lock";
/// Held by the one `addr-upload` of the directory.
const UPLOAD_LOCK: &str = "upload.lock";

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

/// What an export is of: one owner's index of one chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportMeta {
    pub chain_id: u64,
    pub owner: Address,
}

/// The log the importer appends to.
pub struct ExportLog {
    dir: PathBuf,
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
    /// is written again). `None` starts a new export, in a directory without one.
    pub fn open(dir: &Path, meta: &ExportMeta, resume_at: Option<u64>) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = DirLock::acquire(dir, WRITER_LOCK, "addr-import")?;
        let meta_path = dir.join(META_FILE);
        let log_path = dir.join(LOG_FILE);
        match resume_at {
            None => {
                ensure!(
                    is_fresh(dir, meta)?,
                    "{} is not empty; a new export needs an empty directory",
                    dir.display()
                );
                write_atomic(&meta_path, &serde_json::to_vec_pretty(meta)?)?;
            }
            Some(_) => {
                let found: ExportMeta = serde_json::from_slice(
                    &std::fs::read(&meta_path)
                        .with_context(|| format!("reading {}", meta_path.display()))?,
                )?;
                ensure!(
                    found == *meta,
                    "{} is an export for chain {}, owner {}; this run is for chain {}, owner {}",
                    dir.display(),
                    found.chain_id,
                    found.owner,
                    meta.chain_id,
                    meta.owner
                );
            }
        }
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
            out: BufWriter::with_capacity(1 << 20, file),
            len,
            broken: false,
            _lock: lock,
        };
        // A run that stopped before writing `committed` leaves it behind the log.
        log.commit(len)?;
        Ok(log)
    }

    pub fn append(&mut self, kind: u8, key: &B256, data: &[u8]) -> Result<()> {
        ensure!(!self.broken, "an earlier write to the export failed");
        let n = u32::try_from(data.len()).context("record over 4 GiB")?;
        self.broken = true;
        self.out.write_all(&[kind])?;
        self.out.write_all(key.as_slice())?;
        self.out.write_all(&n.to_be_bytes())?;
        self.out.write_all(data)?;
        self.broken = false;
        self.len += RECORD_HEADER + data.len() as u64;
        Ok(())
    }

    pub fn append_soc(&mut self, kind: u8, owner: &Address, soc: &SignedSoc) -> Result<()> {
        self.append(kind, &soc_address(&soc.identifier, owner), &soc.wire())
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
            Some(WRITER_LOCK) => true,
            Some(COMMITTED_FILE) => std::fs::read_to_string(entry.path())?.trim() == "0",
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

/// Writes a small file durably: a temporary file, synced, renamed, and the directory synced,
/// so that after a crash or a power loss it is either the old or the new content.
pub(super) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    if let Some(dir) = path.parent() {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("syncing {}", dir.display()))?;
    }
    Ok(())
}

/// One record of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Offset of the record in the log.
    pub offset: u64,
    pub kind: u8,
    pub key: B256,
    pub data: Vec<u8>,
}

impl Record {
    fn end(&self) -> u64 {
        self.offset + RECORD_HEADER + self.data.len() as u64
    }
}

/// Reads the records of the log in `dir` from `offset` up to `end`.
pub struct Records {
    reader: BufReader<File>,
    offset: u64,
    end: u64,
}

impl Records {
    pub fn open(dir: &Path, offset: u64, end: u64) -> Result<Self> {
        let path = dir.join(LOG_FILE);
        let mut f = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        f.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            reader: BufReader::with_capacity(1 << 20, f),
            offset,
            end,
        })
    }

    pub fn next_record(&mut self) -> Result<Option<Record>> {
        if self.offset >= self.end {
            return Ok(None);
        }
        let mut head = [0u8; RECORD_HEADER as usize];
        self.reader
            .read_exact(&mut head)
            .with_context(|| format!("reading the record at {}", self.offset))?;
        let kind = head[0];
        ensure!(
            matches!(kind, FILE | SOC | HEAD),
            "record of unknown kind {kind} at {}",
            self.offset
        );
        let key = B256::from_slice(&head[1..33]);
        let n = u32::from_be_bytes(head[33..37].try_into().unwrap()) as usize;
        // Checked before allocating: a damaged length must not ask for gigabytes.
        ensure!(
            self.offset + RECORD_HEADER + n as u64 <= self.end,
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
    pub owner: Address,
    pub chain_id: u64,
    pub files: u64,
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

async fn upload_one(
    bee: Bee,
    batch: B256,
    owner: Address,
    rec: Record,
    soc: Option<SignedSoc>,
) -> Result<()> {
    let got = match soc {
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

use super::import::join_one;

/// Uploads the export in `dir` through `bee` with `batch`, from where the last upload of
/// this directory stopped (or from the start), up to its committed length. Records between
/// heads go up in parallel; each head waits for everything before it, then the progress is
/// saved.
pub async fn upload(
    bee: &Bee,
    batch: B256,
    dir: &Path,
    concurrency: usize,
    from_start: bool,
) -> Result<UploadSummary> {
    let _lock = DirLock::acquire(dir, UPLOAD_LOCK, "addr-upload")?;
    let meta: ExportMeta = serde_json::from_slice(
        &std::fs::read(dir.join(META_FILE))
            .with_context(|| format!("{} holds no export", dir.display()))?,
    )?;
    let committed: u64 = std::fs::read_to_string(dir.join(COMMITTED_FILE))
        .context("reading the committed length")?
        .trim()
        .parse()
        .context("parsing the committed length")?;
    let progress_path = dir.join(PROGRESS_FILE);
    let mut progress: Progress = match (from_start, std::fs::read(&progress_path)) {
        (false, Ok(bytes)) => serde_json::from_slice(&bytes)?,
        (false, Err(e)) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("reading {}", progress_path.display()));
        }
        _ => Progress::default(),
    };
    ensure!(
        progress.batch.is_none_or(|b| b == batch),
        "{} was uploaded with batch {} so far; pass --from-start to upload everything with \
         batch {batch}",
        dir.display(),
        progress.batch.unwrap_or_default()
    );
    progress.batch = Some(batch);
    // The log must still hold the head the last upload ended with.
    if let Some(head) = progress.last_head {
        let mut r = Records::open(dir, progress.last_head_at, committed)?;
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
    let mut summary = UploadSummary {
        owner: meta.owner,
        chain_id: meta.chain_id,
        offset: progress.offset,
        committed,
        ..Default::default()
    };
    let mut records = Records::open(dir, progress.offset, committed)?;
    let mut set = tokio::task::JoinSet::new();
    let limit = concurrency.max(1);
    while let Some(rec) = records.next_record()? {
        let soc = check(&rec, &meta.owner)?;
        let (kind, key, offset, end) = (rec.kind, rec.key, rec.offset, rec.end());
        if kind == HEAD {
            while !set.is_empty() {
                join_one(&mut set).await?;
            }
            upload_one(bee.clone(), batch, meta.owner, rec, soc).await?;
            summary.heads += 1;
            progress = Progress {
                offset: end,
                last_head: Some(key),
                last_head_at: offset,
                batch: Some(batch),
            };
            progress.save(&progress_path)?;
        } else {
            if set.len() >= limit {
                join_one(&mut set).await?;
            }
            match kind {
                FILE => summary.files += 1,
                _ => summary.socs += 1,
            }
            set.spawn(upload_one(bee.clone(), batch, meta.owner, rec, soc));
        }
        summary.offset = end;
    }
    while !set.is_empty() {
        join_one(&mut set).await?;
    }
    // Records after the last head (a merge built after it) are uploaded too.
    if summary.offset > progress.offset {
        progress.offset = summary.offset;
        progress.save(&progress_path)?;
    }
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
        let mut r = Records::open(dir, 0, end).unwrap();
        let mut out = Vec::new();
        while let Some(rec) = r.next_record().unwrap() {
            out.push(rec);
        }
        out
    }

    #[test]
    fn log_roundtrip_truncation_and_checks() {
        let dir = tmp("log");
        let signer = PrivateKeySigner::random();
        let meta = ExportMeta {
            chain_id: 1,
            owner: signer.address(),
        };
        let file = vec![7u8; 5000];
        let reference = file_reference(&file);
        let soc = soc::sign(&signer, B256::repeat_byte(1), b"bucket".to_vec()).unwrap();
        let head = soc::sign(&signer, B256::repeat_byte(2), b"head".to_vec()).unwrap();

        let mut log = ExportLog::open(&dir, &meta, None).unwrap();
        log.append(FILE, &reference, &file).unwrap();
        log.append_soc(SOC, &meta.owner, &soc).unwrap();
        let at_unit = log.checkpoint().unwrap();
        log.append_soc(HEAD, &meta.owner, &head).unwrap();
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
        // Resuming from an earlier state drops the records after it.
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
        let meta = ExportMeta {
            chain_id: 1,
            owner: Address::repeat_byte(1),
        };
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
        rec.extend_from_slice(&u32::MAX.to_be_bytes());
        rec.extend_from_slice(&[0u8; 10]);
        std::fs::write(dir.join(LOG_FILE), &rec).unwrap();
        let err = Records::open(&dir, 0, rec.len() as u64)
            .unwrap()
            .next_record()
            .unwrap_err();
        assert!(format!("{err:#}").contains("runs past"), "{err:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
