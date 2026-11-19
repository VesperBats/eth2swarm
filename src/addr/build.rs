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
//! Memory: the sort buffer (given back once the runs are written), one bucket, one page and
//! one path of chunk references of the spill file being written (a spill file above
//! [`STREAM_FILE`] is handed out chunk by chunk as it is written, never whole), and 32 bytes
//! per bucket (for `buckets_root`) and per file (to list each once). A slice that fits one buffer keeps its spool in memory beside it
//! (the records plus 36 bytes per address), so it peaks at up to about twice the buffer.
//! Disk: 45 bytes per appearance for the runs, up to twice that during a merge round (a
//! group is written before its inputs are removed), then the spool.

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
    AddressRecord, BUCKET_TARGET, BucketBody, BucketRecord, Entries, Entry, L_MAX, MAX_FILE,
    PAGE_SIZE, SPILL_INLINE_MAX, SliceRecord, SpillPages, bucket_pages_file, entries_len,
    held_first_block, last_block,
};
use super::slice::{SliceId, bucket_of_prefix, held};
use crate::accumulator::{depth_for, root_of};
use crate::swarm::file::{ChunkTree, chunk_wire, file_reference};

/// Bytes of one sort item.
const ITEM: usize = 12 + 20 + 8 + 4 + 1;
type Item = [u8; ITEM];

/// Runs merged at once; more are merged in rounds.
const FAN_IN: usize = 128;

/// Items or buckets between two looks at the cancel flag.
const CANCEL_EVERY: u64 = 1 << 16;

/// A spill file larger than this is handed out as its chunks while it is written (never held
/// whole); a smaller one whole, as one file.
pub const STREAM_FILE: usize = 1 << 20;

/// What building hands out, in order: files (or a large file's chunks) before the buckets that
/// name them.
#[derive(Debug)]
pub enum Output {
    /// A whole spill or pages file under its reference.
    File(B256, Vec<u8>),
    /// One chunk of a large spill file, under its address, in wire form (`span ‖ payload`).
    /// The file's root chunk comes last.
    Chunk(B256, Vec<u8>),
    Bucket(u32, Vec<u8>),
}

/// The spill file of the address being read from the merge, written page by page: kept whole
/// while it is small, else chunked as it grows.
#[derive(Default)]
struct Spill {
    pages: SpillPages,
    /// The full pages so far, while the file is at most [`STREAM_FILE`].
    file: Vec<u8>,
    /// Once the file is larger: its chunk tree, every chunk handed out as it is made.
    tree: Option<ChunkTree>,
    count: u64,
}

impl Spill {
    fn push(
        &mut self,
        e: &Entry,
        stream_at: usize,
        emit: &mut dyn FnMut(Output) -> Result<()>,
    ) -> Result<()> {
        let Spill {
            pages, file, tree, ..
        } = self;
        pages.push(e, &mut |page| add_page(file, tree, page, stream_at, emit))?;
        self.count += 1;
        Ok(())
    }

    /// The file's reference and entry count, and the file itself unless it was handed out as
    /// chunks.
    fn finish(
        self,
        stream_at: usize,
        emit: &mut dyn FnMut(Output) -> Result<()>,
    ) -> Result<(B256, u64, Option<Vec<u8>>)> {
        let Spill {
            pages,
            mut file,
            mut tree,
            count,
        } = self;
        pages.finish(&mut |page| add_page(&mut file, &mut tree, page, stream_at, emit))?;
        match tree {
            None => Ok((file_reference(&file), count, Some(file))),
            Some(t) => Ok((t.finish(&mut chunk_out(emit))?, count, None)),
        }
    }
}

/// A finished page (a leaf of the file: a page is one chunk) into the file kept whole, or into
/// its chunk tree once the file outgrows `stream_at`.
fn add_page(
    file: &mut Vec<u8>,
    tree: &mut Option<ChunkTree>,
    page: &[u8],
    stream_at: usize,
    emit: &mut dyn FnMut(Output) -> Result<()>,
) -> Result<()> {
    match tree {
        Some(t) => t.push_leaf(page, &mut chunk_out(emit)),
        None => {
            file.extend_from_slice(page);
            if file.len() > stream_at {
                // Every page but the last is full, so the pages so far are whole leaves.
                let mut t = ChunkTree::default();
                for leaf in file.chunks(PAGE_SIZE) {
                    t.push_leaf(leaf, &mut chunk_out(emit))?;
                }
                file.clear();
                file.shrink_to_fit();
                *tree = Some(t);
            }
            Ok(())
        }
    }
}

/// A chunk sink handing chunks out as [`Output::Chunk`].
fn chunk_out<'a>(
    emit: &'a mut dyn FnMut(Output) -> Result<()>,
) -> impl FnMut(B256, u64, &[u8]) -> Result<()> + 'a {
    move |address, span, payload| emit(Output::Chunk(address, chunk_wire(span, payload)))
}

/// One address's entries as they come from the merge: inline while they fit
/// [`SPILL_INLINE_MAX`], then a spill file.
enum Acc {
    Inline(Vec<Entry>, usize),
    Spill(Box<Spill>),
}

impl Acc {
    fn push(
        &mut self,
        e: Entry,
        first: u64,
        stream_at: usize,
        emit: &mut dyn FnMut(Output) -> Result<()>,
    ) -> Result<()> {
        match self {
            Acc::Inline(entries, bytes) => {
                let prev = entries.last().map_or(first, |x| x.block);
                *bytes += entries_len(std::slice::from_ref(&e), prev);
                entries.push(e);
                if *bytes > SPILL_INLINE_MAX {
                    let mut spill = Box::<Spill>::default();
                    for x in entries.iter() {
                        spill.push(x, stream_at, emit)?;
                    }
                    *self = Acc::Spill(spill);
                }
                Ok(())
            }
            Acc::Spill(spill) => spill.push(&e, stream_at, emit),
        }
    }
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
        // `create_dir`, not `create_dir_all`: a path that is already there is not ours.
        std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
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
    /// Spill files above this many bytes are handed out as chunks ([`STREAM_FILE`]).
    stream_file: usize,
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
            stream_file: STREAM_FILE,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Hands out spill files above `bytes` as chunks (default [`STREAM_FILE`]); a test hook
    /// for small slices.
    pub fn with_stream_file(mut self, bytes: usize) -> Self {
        self.stream_file = bytes;
        self
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
            // From here on only the runs are read: give the buffer back.
            self.buf = Vec::new();
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
        let stream_at = self.stream_file;
        let mut flush =
            |key: [u8; 32], acc: Acc, emit: &mut dyn FnMut(Output) -> Result<()>| -> Result<()> {
                addresses += 1;
                let h: [u8; 12] = key[..12].try_into().unwrap();
                let fp: [u8; 8] = h[4..12].try_into().unwrap();
                let rec = match acc {
                    Acc::Inline(entries, _) => {
                        appearances += entries.len() as u64;
                        AddressRecord {
                            fp,
                            entries: Entries::Inline(entries),
                        }
                    }
                    Acc::Spill(spill) => {
                        let (spill_ref, count, whole) = spill.finish(stream_at, emit)?;
                        appearances += count;
                        // A whole file is listed once (two addresses can have the same entries);
                        // a streamed one was handed out as it was written, so two identical
                        // streamed files (identical entries above STREAM_FILE, which hardly
                        // ever happens) upload their chunks twice. Uploads are idempotent.
                        if let Some(file) = whole
                            && seen.insert(spill_ref)
                        {
                            emit(Output::File(spill_ref, file))?;
                        }
                        spilled += 1;
                        AddressRecord {
                            fp,
                            entries: Entries::Spilled { count, spill_ref },
                        }
                    }
                };
                let mut bytes = Vec::new();
                rec.encode(&mut bytes, first);
                total += bytes.len();
                spool.write_all(&key)?;
                spool.write_all(&(bytes.len() as u32).to_be_bytes())?;
                spool.write_all(&bytes)?;
                Ok(())
            };
        // The address being read, its entries so far, and its last entry, which an item of
        // the same block and slot can still add roles to.
        let mut current: Option<([u8; 32], Acc)> = None; // h[0..12] ‖ address
        let mut pending: Option<Entry> = None;
        let mut items = 0u64;
        while let Some(item) = source.next_item()? {
            items += 1;
            if items.is_multiple_of(CANCEL_EVERY) {
                check(&self.cancel)?;
            }
            let key: [u8; 32] = item[..32].try_into().unwrap();
            let block = u64::from_be_bytes(item[32..40].try_into().unwrap());
            let slot = u32::from_be_bytes(item[40..44].try_into().unwrap());
            if current.as_ref().map(|c| c.0) != Some(key) {
                if let Some((k, mut acc)) = current.take() {
                    if let Some(e) = pending.take() {
                        acc.push(e, first, stream_at, emit)?;
                    }
                    flush(k, acc, emit)?;
                }
                current = Some((key, Acc::Inline(Vec::new(), 0)));
            }
            match &mut pending {
                Some(e) if (e.block, e.slot) == (block, slot) => e.roles |= item[44],
                _ => {
                    let next = Entry {
                        block,
                        slot,
                        roles: item[44],
                    };
                    if let Some(e) = pending.replace(next) {
                        let acc = &mut current.as_mut().expect("an address is being read").1;
                        acc.push(e, first, stream_at, emit)?;
                    }
                }
            }
        }
        if let Some((k, mut acc)) = current.take() {
            if let Some(e) = pending.take() {
                acc.push(e, first, stream_at, emit)?;
            }
            flush(k, acc, emit)?;
        }
        // The runs are read: close them and free their disk while the spool is read.
        if let Source::Runs(m) = source {
            m.remove()?;
        }

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
                    ensure!(
                        file.len() <= MAX_FILE,
                        "slice ({level}, {index}): bucket {k} needs a pages file of {} bytes, \
                         above the {MAX_FILE} a client reads",
                        file.len()
                    );
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
        let depth = depth_for(leaves.len());
        let buckets_root = root_of(leaves, depth);
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
    paths: Vec<PathBuf>,
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
                m.remove()?;
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
        Ok(Self {
            readers,
            heap,
            paths: runs.to_vec(),
        })
    }

    /// Closes the runs and removes them.
    fn remove(self) -> Result<()> {
        drop(self.readers);
        for p in &self.paths {
            std::fs::remove_file(p).with_context(|| format!("removing {}", p.display()))?;
        }
        Ok(())
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

    /// Files or chunks by reference or address.
    type Blobs = Vec<(B256, Vec<u8>)>;

    /// Builds `apps` into slice (0, 1), with spill files above `stream_at` streamed: the
    /// descriptor, the buckets, the files and the chunks.
    fn build(apps: &[Appearance], stream_at: usize) -> (Built, Vec<Vec<u8>>, Blobs, Blobs) {
        let mut b = SliceBuilder::new((0, 1), 0, 0xdf, &std::env::temp_dir(), 1 << 20)
            .unwrap()
            .with_stream_file(stream_at);
        for a in apps {
            b.push(a).unwrap();
        }
        let (mut buckets, mut files, mut chunks) = (Vec::new(), Vec::new(), Vec::new());
        let built = b
            .finish(B256::ZERO, None, &mut |out| {
                match out {
                    Output::File(r, f) => files.push((r, f)),
                    Output::Chunk(a, w) => chunks.push((a, w)),
                    Output::Bucket(_, p) => buckets.push(p),
                }
                Ok(())
            })
            .unwrap();
        (built, buckets, files, chunks)
    }

    /// The bytes below `r` among `chunks` (wire form), walking the tree as a reader does.
    fn read_tree(chunks: &std::collections::HashMap<B256, Vec<u8>>, r: &B256, out: &mut Vec<u8>) {
        let w = &chunks[r];
        let span = u64::from_le_bytes(w[..8].try_into().unwrap());
        assert_eq!(crate::swarm::bmt::cac_address(span, &w[8..]), *r);
        if span as usize <= PAGE_SIZE {
            out.extend_from_slice(&w[8..]);
        } else {
            for c in w[8..].chunks(32) {
                read_tree(chunks, &B256::from_slice(c), out);
            }
        }
    }

    #[test]
    fn a_large_spill_file_is_streamed_as_the_chunks_of_the_same_file() {
        // A busy address of 3000 entries (three pages, some entries sharing a block and
        // slot, whose roles merge) and a quiet one.
        let busy = Address::repeat_byte(0x42);
        let mut apps: Vec<Appearance> = (0..3000u64)
            .map(|i| Appearance {
                block: 64 + i / 50,
                slot: (i % 50) as u32 + 1,
                address: busy,
                roles: 1,
            })
            .collect();
        apps.push(Appearance {
            block: 64,
            slot: 1,
            address: busy,
            roles: 4,
        });
        apps.push(Appearance {
            block: 70,
            slot: 2,
            address: Address::repeat_byte(0x07),
            roles: 2,
        });
        let (whole, buckets_whole, files, none) = build(&apps, STREAM_FILE);
        assert!(none.is_empty());
        assert_eq!(files.len(), 1);
        let file = &files[0].1;
        assert_eq!(pages_of(file), 3);
        for stream_at in [0, PAGE_SIZE, 2 * PAGE_SIZE] {
            let (streamed, buckets, files, chunks) = build(&apps, stream_at);
            assert!(files.is_empty(), "stream at {stream_at}");
            // The same buckets (the record names the same reference), the same descriptor.
            assert_eq!(buckets, buckets_whole, "stream at {stream_at}");
            assert_eq!(streamed.record, whole.record);
            // The chunks are the file's: its root last, the tree read back gives the file.
            let root = chunks.last().unwrap().0;
            assert_eq!(root, file_reference(file));
            let map: std::collections::HashMap<_, _> = chunks.into_iter().collect();
            let mut back = Vec::new();
            read_tree(&map, &root, &mut back);
            assert_eq!(&back, file, "stream at {stream_at}");
        }
    }

    fn pages_of(file: &[u8]) -> usize {
        file.len().div_ceil(PAGE_SIZE)
    }

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
                    Output::File(_, f) | Output::Chunk(_, f) => bytes += f.len() as u64,
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
