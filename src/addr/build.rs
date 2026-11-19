//! Building one slice with bounded memory (`docs/address-index.md`, "Building large slices"):
//! the appearances go through an external sort, and buckets and files are handed out as they
//! are made. The result is byte for byte what building in memory gives.
//!
//! 1. **Ingest.** Each appearance becomes a fixed-size item `h[0..12] ‖ address ‖ block ‖
//!    slot ‖ roles` (h = keccak256 of the address), whose byte order is the order needed:
//!    by bucket prefix and fingerprint, then by address, then by entry. Full buffers are
//!    sorted and written to disk as runs.
//! 2. **Records.** A k-way merge of the runs yields each address's entries together. Each
//!    address becomes its record (a busy one spills to a file, handed out at once), written to
//!    a spool in hash order. Only now is the total size, and so the bucket count, known.
//! 3. **Buckets.** One pass over the spool: the bucket number `(h[0..4] · B) >> 32` never
//!    decreases along it, so each bucket's records are consecutive. They are ordered by
//!    (fingerprint, address), encoded, paged if too large, and handed out.
//!
//! Memory: the sort buffer, one bucket, the entries and spill file of the busiest address,
//! and 32 bytes per bucket (for `buckets_root`) and per file (to list each once).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloy::primitives::{B256, keccak256};
use anyhow::{Context, Result, ensure};

use super::detect::Appearance;
use super::layout::{
    AddressRecord, BUCKET_TARGET, BucketBody, BucketRecord, Entries, Entry, L_MAX, PAGE_SIZE,
    SPILL_INLINE_MAX, SliceRecord, bucket_pages_file, entries_len, held_first_block, last_block,
    spill_file,
};
use super::slice::{SliceId, held};
use crate::accumulator::{Tree, depth_for};
use crate::swarm::file::file_reference;

/// Bytes of one sort item.
const ITEM: usize = 12 + 20 + 8 + 4 + 1;
type Item = [u8; ITEM];

/// Runs merged at once; more are merged in rounds.
const FAN_IN: usize = 128;

/// Items or buckets between two looks at the cancel flag.
const CANCEL_EVERY: u64 = 1 << 16;

/// What building hands out, in order: files before the buckets that name them.
#[derive(Debug)]
pub enum Output {
    File(B256, Vec<u8>),
    Bucket(u32, Vec<u8>),
}

/// The descriptor and counts of a built slice; its buckets and files went to the caller.
#[derive(Debug, Clone)]
pub struct Built {
    pub record: SliceRecord,
    pub spilled: u32,
    pub paged: u32,
}

/// Where the sort runs of one slice go: a directory of its own, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(parent: &Path, slice: SliceId) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = parent.join(format!(
            "slice-{}-{}-{}-{}",
            slice.0,
            slice.1,
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Builds one slice from appearances pushed in any order.
pub struct SliceBuilder {
    slice: SliceId,
    first: u64,
    last: u64,
    roles: u8,
    buf: Vec<Item>,
    cap: usize,
    tmp_parent: PathBuf,
    tmp: Option<TempDir>,
    runs: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
}

fn check(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "the slice build was cancelled"
    );
    Ok(())
}

impl SliceBuilder {
    /// A builder for slice `slice` of an index from unit `first_unit` that detects `roles`.
    /// Sort runs of about `buffer_bytes` go to a directory of their own under `tmp_parent`,
    /// created only if the appearances do not fit in one buffer.
    pub fn new(
        slice: SliceId,
        first_unit: u64,
        roles: u8,
        tmp_parent: &Path,
        buffer_bytes: usize,
    ) -> Result<Self> {
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
        Ok(Self {
            slice,
            first,
            last,
            roles,
            buf: Vec::new(),
            cap: (buffer_bytes / ITEM).max(1),
            tmp_parent: tmp_parent.to_path_buf(),
            tmp: None,
            runs: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// A flag that, once set, makes the build stop at its next look (it looks every
    /// [`CANCEL_EVERY`] items or buckets).
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Whether the next [`push`](Self::push) writes a run to disk (a caller on an async
    /// runtime does that one off the runtime's threads).
    pub fn push_spills(&self) -> bool {
        self.buf.len() + 1 >= self.cap
    }

    pub fn push(&mut self, a: &Appearance) -> Result<()> {
        let (level, index) = self.slice;
        ensure!(
            a.roles != 0 && a.roles & !self.roles == 0,
            "appearance with roles {:#04x} outside the slice's {:#04x}",
            a.roles,
            self.roles
        );
        ensure!(
            (self.first..=self.last).contains(&a.block),
            "appearance in block {} outside slice ({level}, {index}), blocks {}..={}",
            a.block,
            self.first,
            self.last
        );
        let h = keccak256(a.address.as_slice());
        let mut item = [0u8; ITEM];
        item[..12].copy_from_slice(&h[..12]);
        item[12..32].copy_from_slice(a.address.as_slice());
        item[32..40].copy_from_slice(&a.block.to_be_bytes());
        item[40..44].copy_from_slice(&a.slot.to_be_bytes());
        item[44] = a.roles;
        if self.buf.capacity() == 0 {
            // Allocated once at its full size: growing by doubling would end near twice the
            // buffer, with the old allocation held during the copy.
            self.buf.reserve_exact(self.cap);
        }
        self.buf.push(item);
        if self.buf.len() >= self.cap {
            self.spill_run()?;
        }
        Ok(())
    }

    /// Sorts the buffer and writes it to disk as a run.
    fn spill_run(&mut self) -> Result<()> {
        self.buf.sort_unstable();
        if self.tmp.is_none() {
            self.tmp = Some(TempDir::new(&self.tmp_parent, self.slice)?);
        }
        let dir = &self.tmp.as_ref().expect("created above").0;
        let path = dir.join(format!("run-{}", self.runs.len()));
        write_run(&path, &self.buf)?;
        self.runs.push(path);
        self.buf.clear();
        Ok(())
    }

    /// Builds the slice: every file and bucket goes to `emit` as it is made, in order (files
    /// before the buckets that name them, buckets by number), and the descriptor is returned.
    /// `force_buckets` overrides the bucket count (tests).
    pub fn finish(
        mut self,
        last_block_hash: B256,
        force_buckets: Option<u32>,
        emit: &mut dyn FnMut(Output) -> Result<()>,
    ) -> Result<Built> {
        let mut source = if self.runs.is_empty() {
            self.buf.sort_unstable();
            Source::Memory(std::mem::take(&mut self.buf).into_iter())
        } else {
            if !self.buf.is_empty() {
                self.spill_run()?;
            }
            let dir = self.tmp.as_ref().expect("runs were written").0.clone();
            Source::Runs(Merge::open(
                &dir,
                std::mem::take(&mut self.runs),
                &self.cancel,
            )?)
        };
        let (level, index) = self.slice;
        let (first, last) = (self.first, self.last);

        // Records, in hash order, to the spool (in memory if the appearances fit one buffer,
        // else on disk next to the runs); spill files out at once.
        let mut spool = match &self.tmp {
            None => Spool::Memory(Vec::new()),
            Some(t) => {
                let path = t.0.join("records");
                Spool::File(
                    BufWriter::with_capacity(1 << 20, File::create(&path)?),
                    path,
                )
            }
        };
        let mut seen = HashSet::new();
        let (mut addresses, mut appearances, mut total, mut spilled) = (0u64, 0u64, 0usize, 0u32);
        let mut current: Option<[u8; 32]> = None; // h[0..12] ‖ address
        let mut entries: Vec<Entry> = Vec::new();
        let mut flush = |key: [u8; 32], entries: &mut Vec<Entry>| -> Result<()> {
            appearances += entries.len() as u64;
            addresses += 1;
            let h: [u8; 12] = key[..12].try_into().unwrap();
            let fp: [u8; 8] = h[4..12].try_into().unwrap();
            let rec = if entries_len(entries, first) > SPILL_INLINE_MAX {
                let file = spill_file(entries);
                let spill_ref = file_reference(&file);
                if seen.insert(spill_ref) {
                    emit(Output::File(spill_ref, file))?;
                }
                spilled += 1;
                AddressRecord {
                    fp,
                    entries: Entries::Spilled {
                        count: entries.len() as u64,
                        spill_ref,
                    },
                }
            } else {
                AddressRecord {
                    fp,
                    entries: Entries::Inline(std::mem::take(entries)),
                }
            };
            entries.clear();
            let mut bytes = Vec::new();
            rec.encode(&mut bytes, first);
            total += bytes.len();
            spool.write_all(&key)?;
            spool.write_all(&(bytes.len() as u32).to_be_bytes())?;
            spool.write_all(&bytes)?;
            Ok(())
        };
        let mut items = 0u64;
        while let Some(item) = source.next_item()? {
            items += 1;
            if items.is_multiple_of(CANCEL_EVERY) {
                check(&self.cancel)?;
            }
            let key: [u8; 32] = item[..32].try_into().unwrap();
            if current != Some(key) {
                if let Some(k) = current {
                    flush(k, &mut entries)?;
                }
                current = Some(key);
            }
            let block = u64::from_be_bytes(item[32..40].try_into().unwrap());
            let slot = u32::from_be_bytes(item[40..44].try_into().unwrap());
            match entries.last_mut() {
                Some(e) if (e.block, e.slot) == (block, slot) => e.roles |= item[44],
                _ => entries.push(Entry {
                    block,
                    slot,
                    roles: item[44],
                }),
            }
        }
        if let Some(k) = current {
            flush(k, &mut entries)?;
        }
        drop(source);

        let bucket_count = match force_buckets {
            Some(b) => b,
            None => u32::try_from(total.div_ceil(BUCKET_TARGET).max(1))?,
        };

        // Buckets, in order, from one pass over the spool.
        let mut reader = spool.into_reader()?;
        let mut leaves = Vec::with_capacity(bucket_count as usize);
        let mut paged = 0u32;
        // A bucket's records as encoded, with their (fingerprint, address) order key; they are
        // decoded only for a bucket that has to be paged.
        let mut group: Vec<([u8; 8], [u8; 20], Vec<u8>)> = Vec::new();
        let mut group_k = 0u32;
        let cancel = self.cancel.clone();
        let mut emit_bucket =
            |k: u32, group: &mut Vec<([u8; 8], [u8; 20], Vec<u8>)>| -> Result<()> {
                if u64::from(k).is_multiple_of(CANCEL_EVERY) {
                    check(&cancel)?;
                }
                group.sort_by_key(|g| (g.0, g.1));
                let mut payload =
                    BucketRecord::encode_inline(level, index, k, group.iter().map(|g| &g.2[..]));
                if payload.len() > PAGE_SIZE {
                    let recs = group
                        .iter()
                        .map(|g| AddressRecord::decode(&mut &g.2[..], first, last))
                        .collect::<Result<Vec<_>>>()?;
                    let file = bucket_pages_file(&recs, first);
                    let pages_ref = file_reference(&file);
                    payload = BucketRecord {
                        level,
                        index,
                        bucket: k,
                        base: first,
                        body: BucketBody::Paged {
                            count: recs.len() as u32,
                            pages: file.len().div_ceil(PAGE_SIZE) as u32,
                            pages_ref,
                        },
                    }
                    .encode();
                    if seen.insert(pages_ref) {
                        emit(Output::File(pages_ref, file))?;
                    }
                    paged += 1;
                }
                group.clear();
                leaves.push(keccak256(&payload));
                emit(Output::Bucket(k, payload))
            };
        loop {
            let mut key = [0u8; 32];
            match reader.read_exact(&mut key) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let mut len = [0u8; 4];
            reader.read_exact(&mut len)?;
            let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
            reader.read_exact(&mut bytes)?;
            let k = bucket_of_prefix(
                u32::from_be_bytes(key[..4].try_into().unwrap()),
                bucket_count,
            );
            while group_k < k {
                emit_bucket(group_k, &mut group)?;
                group_k += 1;
            }
            let fp: [u8; 8] = key[4..12].try_into().unwrap();
            let address: [u8; 20] = key[12..32].try_into().unwrap();
            group.push((fp, address, bytes));
        }
        while group_k < bucket_count {
            emit_bucket(group_k, &mut group)?;
            group_k += 1;
        }
        drop(emit_bucket);
        let buckets_root = Tree::new(&leaves, depth_for(leaves.len())).root();
        Ok(Built {
            record: SliceRecord {
                level,
                index,
                first_block: first,
                last_block_hash,
                bucket_count,
                addresses: u32::try_from(addresses)?,
                appearances,
                buckets_root,
                roles: self.roles,
            },
            spilled,
            paged,
        })
    }
}

/// The records of a slice between the two passes.
enum Spool {
    Memory(Vec<u8>),
    File(BufWriter<File>, PathBuf),
}

impl Spool {
    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        match self {
            Spool::Memory(v) => v.extend_from_slice(data),
            Spool::File(w, _) => w.write_all(data)?,
        }
        Ok(())
    }

    fn into_reader(self) -> Result<Box<dyn Read>> {
        Ok(match self {
            Spool::Memory(v) => Box::new(std::io::Cursor::new(v)),
            Spool::File(mut w, path) => {
                w.flush()?;
                drop(w);
                Box::new(BufReader::with_capacity(1 << 20, File::open(path)?))
            }
        })
    }
}

/// Bucket of an address whose hash starts with `prefix`, among `buckets` (multiply-shift, as
/// `slice::bucket_of`).
pub fn bucket_of_prefix(prefix: u32, buckets: u32) -> u32 {
    ((prefix as u64 * buckets as u64) >> 32) as u32
}

fn write_run(path: &Path, items: &[Item]) -> Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
    for item in items {
        w.write_all(item)?;
    }
    w.flush()?;
    Ok(())
}

fn read_item(r: &mut BufReader<File>) -> Result<Option<Item>> {
    let mut item = [0u8; ITEM];
    match r.read_exact(&mut item) {
        Ok(()) => Ok(Some(item)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The sorted items: the one buffer, or a merge of the runs.
enum Source {
    Memory(std::vec::IntoIter<Item>),
    Runs(Merge),
}

impl Source {
    fn next_item(&mut self) -> Result<Option<Item>> {
        match self {
            Source::Memory(it) => Ok(it.next()),
            Source::Runs(m) => m.next_item(),
        }
    }
}

/// A k-way merge of sorted runs.
struct Merge {
    readers: Vec<BufReader<File>>,
    heap: BinaryHeap<Reverse<(Item, usize)>>,
}

impl Merge {
    /// Merges `runs`, first in rounds of [`FAN_IN`] into new runs in `dir` if there are more.
    fn open(dir: &Path, mut runs: Vec<PathBuf>, cancel: &AtomicBool) -> Result<Self> {
        let mut round = 0;
        while runs.len() > FAN_IN {
            let mut next = Vec::new();
            for (i, group) in runs.chunks(FAN_IN).enumerate() {
                let path = dir.join(format!("merge-{round}-{i}"));
                let mut m = Self::of(group)?;
                let mut w = BufWriter::with_capacity(1 << 20, File::create(&path)?);
                let mut n = 0u64;
                while let Some(item) = m.next_item()? {
                    w.write_all(&item)?;
                    n += 1;
                    if n.is_multiple_of(CANCEL_EVERY) {
                        check(cancel)?;
                    }
                }
                w.flush()?;
                drop(m);
                for p in group {
                    std::fs::remove_file(p)?;
                }
                next.push(path);
            }
            runs = next;
            round += 1;
        }
        Self::of(&runs)
    }

    fn of(runs: &[PathBuf]) -> Result<Self> {
        let mut readers = Vec::with_capacity(runs.len());
        let mut heap = BinaryHeap::with_capacity(runs.len());
        for (i, path) in runs.iter().enumerate() {
            let mut r = BufReader::with_capacity(1 << 16, File::open(path)?);
            if let Some(item) = read_item(&mut r)? {
                heap.push(Reverse((item, i)));
            }
            readers.push(r);
        }
        Ok(Self { readers, heap })
    }

    fn next_item(&mut self) -> Result<Option<Item>> {
        let Some(Reverse((item, i))) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some(next) = read_item(&mut self.readers[i])? {
            self.heap.push(Reverse((next, i)));
        }
        Ok(Some(item))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    fn peak_rss_kb() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmHWM:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
            })
            .unwrap_or(0)
    }

    /// Memory stays bounded by the sort buffer whatever the slice's size. Not run by default
    /// (about a minute): `cargo test --release memory_is_bounded -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn memory_is_bounded() {
        let n: u64 = std::env::var("APPEARANCES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20_000_000);
        let slice = (10, 25); // 65,536 blocks
        let first = 25 * 1024 * 64;
        let mut b = SliceBuilder::new(slice, 0, 0x7f, &std::env::temp_dir(), 64 << 20).unwrap();
        let start = std::time::Instant::now();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for i in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Zipf-like: a few busy addresses, a long tail.
            let a = if x % 10 < 3 {
                x % 1000
            } else {
                x % (n / 4).max(1)
            };
            b.push(&Appearance {
                block: first + i * 65_536 / n,
                slot: (x >> 40) as u32 % 300,
                address: Address::left_padding_from(&a.to_be_bytes()),
                roles: 1 << ((x >> 33) % 7),
            })
            .unwrap();
        }
        let ingest = start.elapsed();
        let (mut buckets, mut bytes) = (0u64, 0u64);
        let built = b
            .finish(B256::ZERO, None, &mut |out| {
                match out {
                    Output::File(_, f) => bytes += f.len() as u64,
                    Output::Bucket(_, p) => {
                        buckets += 1;
                        bytes += p.len() as u64
                    }
                }
                Ok(())
            })
            .unwrap();
        println!(
            "{n} appearances, {} addresses: ingest {ingest:?}, total {:?}, {buckets} buckets, \
             {:.2} bytes/appearance published, peak RSS {} MiB",
            built.record.addresses,
            start.elapsed(),
            bytes as f64 / n as f64,
            peak_rss_kb() / 1024
        );
    }
}
