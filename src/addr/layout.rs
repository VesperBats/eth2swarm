//! `addr1` on Swarm: identifiers, parameters and the binary records of the address index
//! (`docs/address-index.md`, "Identifiers" and "Record formats").
//!
//! Encodings are canonical: the same appearances give byte-identical records and pages, so
//! independent importers can be compared. Decoders reject what an encoder would not produce
//! as far as one record or page shows it: non-shortest varints, unsorted entries or records,
//! empty roles, an inline record that should have been spilled, an inline bucket that should
//! have been paged, empty spill or paged counts, non-zero padding. Whether a spill or pages
//! file's content is canonical shows only when it is read.

use alloy::primitives::{B256, keccak256};
use anyhow::{Context, Result, bail, ensure};

use crate::layout::{Reader, Writer};

/// Blocks per unit (`U`).
pub const UNIT_BLOCKS: u64 = 64;
/// Top level (`L_max`): slices of `2^L_MAX` units are never merged again.
pub const L_MAX: u8 = 16;
/// Bucket target `T`: average address-record bytes per bucket.
pub const BUCKET_TARGET: usize = 2816;
/// Inline entry bytes of one address above which it is spilled to its own file.
pub const SPILL_INLINE_MAX: usize = 256;
/// Bucket payload limit (one chunk) and size of spill and bucket pages.
pub const PAGE_SIZE: usize = 4096;
/// Largest spill or pages file a client reads whole. A larger spill file is written as it is
/// built and read page by page; where it cannot be (an export started before layout 3), and
/// for a pages file, the importer refuses to make a larger one.
pub const MAX_FILE: usize = 256 << 20;

const KIND_SLICE: u8 = 4;
const KIND_BUCKET: u8 = 5;
const KIND_HEAD: u8 = 6;

/// First block of slice `(level, index)`.
pub fn first_block(level: u8, index: u32) -> u64 {
    ((index as u64) << level) * UNIT_BLOCKS
}

/// Last block of slice `(level, index)`.
pub fn last_block(level: u8, index: u32) -> u64 {
    ((index as u64 + 1) << level) * UNIT_BLOCKS - 1
}

/// First block slice `(level, index)` holds in an index that starts at unit `first_unit`: its
/// own first block, or the index's for the slice the start falls into. Only for a slice that
/// exists in that index, i.e. ends after `first_unit` (`slice::held` is not empty).
pub fn held_first_block(level: u8, index: u32, first_unit: u64) -> u64 {
    debug_assert!(
        ((index as u64 + 1) << level) > first_unit,
        "slice ({level}, {index}) ends before the index starts at unit {first_unit}"
    );
    first_block(level, index).max(first_unit * UNIT_BLOCKS)
}

pub fn slice_id(chain_id: u64, level: u8, index: u32) -> B256 {
    keccak256(format!("eth:{chain_id}:addr1:slice:{level}:{index}"))
}

pub fn bucket_id(chain_id: u64, level: u8, index: u32, bucket: u32) -> B256 {
    keccak256(format!(
        "eth:{chain_id}:addr1:bucket:{level}:{index}:{bucket}"
    ))
}

pub fn head_topic(chain_id: u64) -> B256 {
    keccak256(format!("eth:{chain_id}:addr1:head"))
}

/// Head feed update `n` (Bee's sequence-feed identifier scheme).
pub fn head_id(chain_id: u64, n: u64) -> B256 {
    let mut buf = [0u8; 40];
    buf[..32].copy_from_slice(head_topic(chain_id).as_slice());
    buf[32..].copy_from_slice(&n.to_be_bytes());
    keccak256(buf)
}

pub fn origin_id(chain_id: u64) -> B256 {
    keccak256(format!("eth:{chain_id}:addr1:origin"))
}

/// Unsigned LEB128, shortest form.
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn varint_len(v: u64) -> usize {
    (64 - (v | 1).leading_zeros() as usize).div_ceil(7)
}

/// Reads a shortest-form varint; anything longer, or wider than 64 bits, is an error.
pub fn get_varint(buf: &mut &[u8]) -> Result<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let (&b, rest) = buf.split_first().context("varint truncated")?;
        *buf = rest;
        ensure!(i < 9 || b <= 1, "varint wider than 64 bits");
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            ensure!(i == 0 || b != 0, "varint not in shortest form");
            return Ok(v);
        }
    }
    bail!("varint wider than 64 bits")
}

/// One appearance of an address, as stored in a bucket or spill page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Entry {
    pub block: u64,
    pub slot: u32,
    pub roles: u8,
}

/// Encoded size of `entries` with block deltas from `base`.
pub fn entries_len(entries: &[Entry], base: u64) -> usize {
    let mut prev = base;
    entries
        .iter()
        .map(|e| {
            let n = varint_len(e.block - prev) + varint_len(e.slot as u64) + 1;
            prev = e.block;
            n
        })
        .sum()
}

fn put_entries(out: &mut Vec<u8>, entries: &[Entry], base: u64) {
    let mut prev = base;
    for e in entries {
        put_varint(out, e.block - prev);
        put_varint(out, e.slot as u64);
        out.push(e.roles);
        prev = e.block;
    }
}

/// Reads `n` entries; they must be strictly ascending by (block, slot), have non-empty roles,
/// and lie at or before `last` (they cannot lie before `base`, deltas being unsigned).
fn get_entries(buf: &mut &[u8], n: u64, base: u64, last: u64) -> Result<Vec<Entry>> {
    let mut out: Vec<Entry> = Vec::with_capacity(n.min(4096) as usize);
    let mut prev = base;
    for _ in 0..n {
        let block = prev
            .checked_add(get_varint(buf)?)
            .context("entry block overflows")?;
        let slot = u32::try_from(get_varint(buf)?).context("entry slot wider than 32 bits")?;
        let (&roles, rest) = buf.split_first().context("entry truncated")?;
        *buf = rest;
        ensure!(roles != 0, "entry without roles");
        ensure!(
            block <= last,
            "entry in block {block}, after the slice's last block {last}"
        );
        let e = Entry { block, slot, roles };
        if let Some(last) = out.last() {
            ensure!(
                (last.block, last.slot) < (e.block, e.slot),
                "entries not strictly ascending"
            );
        }
        out.push(e);
        prev = block;
    }
    Ok(out)
}

/// The entries of an address: inline, or the reference of its spill file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entries {
    Inline(Vec<Entry>),
    Spilled { count: u64, spill_ref: B256 },
}

/// One address in a bucket. The address itself is not stored, only its fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressRecord {
    pub fp: [u8; 8],
    pub entries: Entries,
}

impl AddressRecord {
    pub fn encode(&self, out: &mut Vec<u8>, base: u64) {
        out.extend_from_slice(&self.fp);
        match &self.entries {
            Entries::Inline(e) => {
                put_varint(out, (e.len() as u64) << 1);
                put_entries(out, e, base);
            }
            Entries::Spilled { count, spill_ref } => {
                put_varint(out, (count << 1) | 1);
                out.extend_from_slice(spill_ref.as_slice());
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn encoded_len(&self, base: u64) -> usize {
        8 + match &self.entries {
            Entries::Inline(e) => varint_len((e.len() as u64) << 1) + entries_len(e, base),
            Entries::Spilled { count, .. } => varint_len((count << 1) | 1) + 32,
        }
    }

    /// Decodes a record of a slice whose blocks are `base..=last`.
    pub fn decode(buf: &mut &[u8], base: u64, last: u64) -> Result<Self> {
        ensure!(buf.len() >= 8, "address record truncated");
        let fp: [u8; 8] = buf[..8].try_into()?;
        *buf = &buf[8..];
        let tag = get_varint(buf)?;
        let n = tag >> 1;
        let entries = if tag & 1 == 1 {
            ensure!(n > 0, "spilled address record without entries");
            ensure!(buf.len() >= 32, "spill reference truncated");
            let spill_ref = B256::from_slice(&buf[..32]);
            *buf = &buf[32..];
            Entries::Spilled {
                count: n,
                spill_ref,
            }
        } else {
            ensure!(n > 0, "address record without entries");
            let entries = get_entries(buf, n, base, last)?;
            ensure!(
                entries_len(&entries, base) <= SPILL_INLINE_MAX,
                "inline entries over {SPILL_INLINE_MAX} bytes (should be spilled)"
            );
            Entries::Inline(entries)
        };
        Ok(Self { fp, entries })
    }
}

/// Records must be ascending by fingerprint (records with equal fingerprints are ordered by
/// address, which a reader cannot see).
fn check_fp_order(records: &[AddressRecord]) -> Result<()> {
    ensure!(
        records.windows(2).all(|w| w[0].fp <= w[1].fp),
        "address records not ascending by fingerprint"
    );
    Ok(())
}

/// Appends one page (`count (u16) ‖ body`) to `file`, zero-padded to [`PAGE_SIZE`] unless it
/// is the last.
fn flush_page(file: &mut Vec<u8>, body: &[u8], count: u16, last: bool) {
    let start = file.len();
    file.extend(count.to_be_bytes());
    file.extend_from_slice(body);
    if !last {
        file.resize(start + PAGE_SIZE, 0);
    }
}

/// Splits a pages file into its pages; only the last one may be shorter than
/// [`PAGE_SIZE`]. The pages' content is checked when each is decoded.
pub fn pages(file: &[u8]) -> Result<Vec<&[u8]>> {
    let pages: Vec<&[u8]> = file.chunks(PAGE_SIZE).collect();
    ensure!(!pages.is_empty(), "empty pages file");
    Ok(pages)
}

/// Checks that what is left of a page after its content is zero padding (and only on a
/// page that is not the last, i.e. a full 4096-byte page).
fn check_padding(rest: &[u8], page_len: usize) -> Result<()> {
    if rest.is_empty() {
        return Ok(());
    }
    ensure!(
        page_len == PAGE_SIZE && rest.iter().all(|b| *b == 0),
        "page has trailing bytes that are not padding"
    );
    Ok(())
}

/// A spill file: an address's entries in pages of `count (u16) ‖ first_block (u32) ‖
/// entries`, filled greedily. On each page the first entry's delta is relative to
/// `first_block` (so 0), every other entry's to the previous entry.
pub fn spill_file(entries: &[Entry]) -> Vec<u8> {
    let mut file = Vec::new();
    let mut pages = SpillPages::default();
    let mut out = |page: &[u8]| -> Result<()> {
        file.extend_from_slice(page);
        Ok(())
    };
    for e in entries {
        pages
            .push(e, &mut out)
            .expect("collecting pages cannot fail");
    }
    pages
        .finish(&mut out)
        .expect("collecting pages cannot fail");
    file
}

/// A spill file built entry by entry ([`spill_file`]'s pages): every full page is handed out
/// padded to [`PAGE_SIZE`] as soon as the next entry does not fit, the last one unpadded at
/// [`SpillPages::finish`]. Memory is one page, whatever the number of entries.
#[derive(Debug, Default)]
pub struct SpillPages {
    /// The page being filled: `first_block ‖ entries`.
    body: Vec<u8>,
    count: u16,
    prev: u64,
}

impl SpillPages {
    /// Adds the next entry (ascending by block and slot).
    pub fn push(&mut self, e: &Entry, out: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let len = if self.count == 0 {
            0
        } else {
            entries_len(std::slice::from_ref(e), self.prev)
        };
        if self.count == 0 || 2 + self.body.len() + len > PAGE_SIZE {
            if self.count > 0 {
                out(&page(&self.body, self.count, false))?;
            }
            self.body.clear();
            self.count = 0;
            let first =
                u32::try_from(e.block).context("block number beyond u32 in a spill page")?;
            self.body.extend(first.to_be_bytes());
            self.prev = e.block;
        }
        put_entries(&mut self.body, std::slice::from_ref(e), self.prev);
        self.count += 1;
        self.prev = e.block;
        Ok(())
    }

    /// Hands out the last page (unpadded), if there is one.
    pub fn finish(self, out: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        if self.count > 0 {
            out(&page(&self.body, self.count, true))?;
        }
        Ok(())
    }
}

/// One page: `count (u16) ‖ body`, zero-padded to [`PAGE_SIZE`] unless it is the last.
fn page(body: &[u8], count: u16, last: bool) -> Vec<u8> {
    let mut p = Vec::with_capacity(PAGE_SIZE);
    flush_page(&mut p, body, count, last);
    p
}

/// A spill file checked page by page, in order: for a file read through its chunk tree
/// rather than whole (its pages are its leaves). It accepts exactly what reading it whole and
/// comparing with [`spill_file`] accepts: every page decodes, the entries ascend across pages,
/// every page but the last is full (the next page's first entry did not fit, the writer's
/// greedy rule) and padded, the last is unpadded and as long as the span says, and the count
/// is the record's. A page that fails leaves the check as it was.
#[derive(Debug, Clone)]
pub struct SpillPageCheck {
    first: u64,
    last: u64,
    count: u64,
    span: u64,
    pages: u64,
    page: u64,
    seen: u64,
    /// The previous page's content length and last entry.
    prev: Option<(usize, Entry)>,
}

impl SpillPageCheck {
    /// For a file of `span` bytes holding the `count` entries of a record of a slice whose
    /// blocks are `first..=last`.
    pub fn new(first: u64, last: u64, count: u64, span: u64) -> Result<Self> {
        ensure!(count > 0 && span > 0, "an empty spill file");
        Ok(Self {
            first,
            last,
            count,
            span,
            pages: span.div_ceil(PAGE_SIZE as u64),
            page: 0,
            seen: 0,
            prev: None,
        })
    }

    /// Checks the next page and returns its entries.
    pub fn page(&mut self, page: &[u8]) -> Result<Vec<Entry>> {
        ensure!(self.page < self.pages, "a spill page after the last one");
        let last_page = self.page + 1 == self.pages;
        let want = if last_page {
            self.span - (self.pages - 1) * PAGE_SIZE as u64
        } else {
            PAGE_SIZE as u64
        };
        ensure!(
            page.len() as u64 == want,
            "spill page {} has {} bytes, the span says {want}",
            self.page,
            page.len()
        );
        let entries = spill_page(page, self.first, self.last, last_page)?;
        let content = 6 + entries_len(&entries, entries[0].block);
        if let Some((prev_content, e)) = self.prev {
            let next = entries[0];
            ensure!(
                (e.block, e.slot) < (next.block, next.slot),
                "spill entries not ascending across pages {} and {}",
                self.page - 1,
                self.page
            );
            ensure!(
                prev_content + entries_len(std::slice::from_ref(&next), e.block) > PAGE_SIZE,
                "spill file not canonically encoded: page {} ends early",
                self.page - 1
            );
        }
        let seen = self.seen + entries.len() as u64;
        ensure!(
            seen <= self.count,
            "spill file holds more than the {} entries the record states",
            self.count
        );
        ensure!(
            !last_page || seen == self.count,
            "spill file holds {seen} entries, the record states {}",
            self.count
        );
        self.prev = Some((content, *entries.last().expect("a page has entries")));
        self.seen = seen;
        self.page += 1;
        Ok(entries)
    }

    /// Whether every page has been checked (and with the last, the count).
    pub fn done(&self) -> bool {
        self.page == self.pages
    }
}

/// One spill page of a slice whose blocks are `first..=last`, read on its own (a client
/// showing a busy address reads the last pages first): a full page unless it is the last,
/// which is unpadded. The checks across pages need [`SpillPageCheck`].
pub fn spill_page(page: &[u8], first: u64, last: u64, last_page: bool) -> Result<Vec<Entry>> {
    ensure!(
        last_page || page.len() == PAGE_SIZE,
        "spill page of {} bytes",
        page.len()
    );
    let entries = decode_spill_page(page, first, last)?;
    ensure!(
        !last_page || 6 + entries_len(&entries, entries[0].block) == page.len(),
        "spill file not canonically encoded: its last page is padded"
    );
    Ok(entries)
}

/// Decodes one spill page of a slice whose blocks are `first..=last`.
pub fn decode_spill_page(page: &[u8], first: u64, last: u64) -> Result<Vec<Entry>> {
    ensure!(page.len() >= 6, "spill page truncated");
    let count = u16::from_be_bytes([page[0], page[1]]);
    ensure!(count > 0, "empty spill page");
    let page_first = u32::from_be_bytes(page[2..6].try_into()?) as u64;
    ensure!(
        page_first >= first,
        "spill page starts at block {page_first}, before the slice's first block {first}"
    );
    let mut rest = &page[6..];
    let entries = get_entries(&mut rest, count as u64, page_first, last)?;
    ensure!(
        entries[0].block == page_first,
        "first entry is not at first_block"
    );
    check_padding(rest, page.len())?;
    Ok(entries)
}

/// A bucket pages file: records in pages of `count (u16) ‖ records`, filled greedily; a
/// record never straddles two pages.
pub fn bucket_pages_file(records: &[AddressRecord], base: u64) -> Vec<u8> {
    let mut file = Vec::new();
    let mut body = Vec::new();
    let mut count = 0u16;
    for rec in records {
        let mut enc = Vec::new();
        rec.encode(&mut enc, base);
        assert!(
            2 + enc.len() <= PAGE_SIZE,
            "address record larger than a page"
        );
        if count > 0 && 2 + body.len() + enc.len() > PAGE_SIZE {
            flush_page(&mut file, &body, count, false);
            body.clear();
            count = 0;
        }
        body.extend(enc);
        count += 1;
    }
    if count > 0 {
        flush_page(&mut file, &body, count, true);
    }
    file
}

/// Decodes one page of a bucket pages file of a slice whose blocks are `base..=last`.
pub fn decode_bucket_page(page: &[u8], base: u64, last: u64) -> Result<Vec<AddressRecord>> {
    ensure!(page.len() >= 2, "bucket page truncated");
    let count = u16::from_be_bytes([page[0], page[1]]);
    ensure!(count > 0, "empty bucket page");
    let mut rest = &page[2..];
    let records = (0..count)
        .map(|_| AddressRecord::decode(&mut rest, base, last))
        .collect::<Result<Vec<_>>>()?;
    check_fp_order(&records)?;
    check_padding(rest, page.len())?;
    Ok(records)
}

/// Descriptor of slice `(level, index)` (kind 4, 111 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceRecord {
    pub level: u8,
    pub index: u32,
    /// The first block the slice holds: [`first_block`] of `(level, index)`, or the index's
    /// first block for the slice its start falls into ([`held_first_block`]).
    pub first_block: u64,
    pub last_block_hash: B256,
    pub bucket_count: u32,
    pub addresses: u32,
    pub appearances: u64,
    pub buckets_root: B256,
    pub roles: u8,
}

impl SliceRecord {
    // Format constant, checked by the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub const LEN: usize = 111;

    pub fn last_block(&self) -> u64 {
        last_block(self.level, self.index)
    }

    pub fn encode(&self) -> Vec<u8> {
        assert!(
            valid_first_block(self.level, self.index, self.first_block),
            "slice ({}, {}) cannot start at block {}",
            self.level,
            self.index,
            self.first_block
        );
        let mut w = Writer::new(KIND_SLICE);
        w.u32(UNIT_BLOCKS as u32);
        w.u8(self.level);
        w.u32(self.index);
        w.u64(self.first_block);
        w.u64(self.last_block());
        w.hash(&self.last_block_hash);
        w.u32(self.bucket_count);
        w.u32(self.addresses);
        w.u64(self.appearances);
        w.hash(&self.buckets_root);
        w.u8(self.roles);
        w.0
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf, KIND_SLICE)?;
        let unit = r.u32()?;
        ensure!(
            unit as u64 == UNIT_BLOCKS,
            "slice unit is {unit} blocks, not {UNIT_BLOCKS}"
        );
        let level = r.u8()?;
        ensure!(level <= L_MAX, "slice level {level} above {L_MAX}");
        let index = r.u32()?;
        let (first, last) = (r.u64()?, r.u64()?);
        ensure!(
            valid_first_block(level, index, first) && last == last_block(level, index),
            "slice ({level}, {index}) states blocks {first}..={last}"
        );
        let rec = Self {
            level,
            index,
            first_block: first,
            last_block_hash: r.hash()?,
            bucket_count: r.u32()?,
            addresses: r.u32()?,
            appearances: r.u64()?,
            buckets_root: r.hash()?,
            roles: r.u8()?,
        };
        ensure!(rec.bucket_count >= 1, "slice without buckets");
        r.finish()?;
        Ok(rec)
    }
}

/// A slice can start at a unit boundary inside its own units (the index's first block), not
/// before them and not after its last unit.
fn valid_first_block(level: u8, index: u32, first: u64) -> bool {
    first.is_multiple_of(UNIT_BLOCKS)
        && (first_block(level, index)..=last_block(level, index)).contains(&first)
}

/// What a bucket holds: its address records, or the reference of the pages file with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BucketBody {
    Inline(Vec<AddressRecord>),
    Paged {
        count: u32,
        pages: u32,
        pages_ref: B256,
    },
}

/// Bucket `bucket` of slice `(level, index)` (kind 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketRecord {
    pub level: u8,
    pub index: u32,
    pub bucket: u32,
    /// The slice's first block (its `SliceRecord`'s `first_block`): the base of the block
    /// deltas. Not part of the encoding.
    pub base: u64,
    pub body: BucketBody,
}

impl BucketRecord {
    /// Header size of an inline bucket (`MAGIC ‖ VERSION ‖ kind ‖ level ‖ index ‖ bucket ‖
    /// paged ‖ count`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub const INLINE_HEADER: usize = 19;

    /// An inline bucket from its address records, each already encoded
    /// ([`AddressRecord::encode`] with the slice's base), in bucket order.
    pub fn encode_inline<'a>(
        level: u8,
        index: u32,
        bucket: u32,
        records: impl ExactSizeIterator<Item = &'a [u8]>,
    ) -> Vec<u8> {
        let mut w = Writer::new(KIND_BUCKET);
        w.u8(level);
        w.u32(index);
        w.u32(bucket);
        w.u8(0);
        w.u32(u32::try_from(records.len()).expect("more than 2^32 records"));
        for rec in records {
            w.0.extend_from_slice(rec);
        }
        w.0
    }

    pub fn encode(&self) -> Vec<u8> {
        let base = self.base;
        if let BucketBody::Inline(records) = &self.body {
            let encoded: Vec<Vec<u8>> = records
                .iter()
                .map(|rec| {
                    let mut out = Vec::new();
                    rec.encode(&mut out, base);
                    out
                })
                .collect();
            return Self::encode_inline(
                self.level,
                self.index,
                self.bucket,
                encoded.iter().map(|r| &r[..]),
            );
        }
        let mut w = Writer::new(KIND_BUCKET);
        w.u8(self.level);
        w.u32(self.index);
        w.u32(self.bucket);
        match &self.body {
            BucketBody::Inline(_) => unreachable!("encoded above"),
            BucketBody::Paged {
                count,
                pages,
                pages_ref,
            } => {
                w.u8(1);
                w.u32(*count);
                w.u32(*pages);
                w.hash(pages_ref);
            }
        }
        w.0
    }

    /// Decodes a bucket from the grid start of the slice its header names: right for an
    /// index from genesis, where every slice starts there; for the slice a later start falls
    /// into, use [`BucketRecord::decode`] with its descriptor's `first_block`. For tools that
    /// look at one bucket without its descriptor.
    pub fn decode_at_grid_start(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf, KIND_BUCKET)?;
        let level = r.u8()?;
        ensure!(level <= L_MAX, "bucket level {level} above {L_MAX}");
        let index = r.u32()?;
        Self::decode(buf, first_block(level, index))
    }

    /// Decodes a bucket of a slice whose first block is `base` (from its `SliceRecord`).
    pub fn decode(buf: &[u8], base: u64) -> Result<Self> {
        ensure!(
            buf.len() <= PAGE_SIZE,
            "bucket of {} bytes (should have been paged)",
            buf.len()
        );
        let mut r = Reader::new(buf, KIND_BUCKET)?;
        let level = r.u8()?;
        ensure!(level <= L_MAX, "bucket level {level} above {L_MAX}");
        let index = r.u32()?;
        let bucket = r.u32()?;
        ensure!(
            valid_first_block(level, index, base),
            "bucket of slice ({level}, {index}) decoded from block {base}"
        );
        let last = last_block(level, index);
        let body = match r.u8()? {
            0 => {
                let count = r.u32()?;
                let mut rest = r.0;
                let records = (0..count)
                    .map(|_| AddressRecord::decode(&mut rest, base, last))
                    .collect::<Result<Vec<_>>>()?;
                r.0 = rest;
                check_fp_order(&records)?;
                BucketBody::Inline(records)
            }
            1 => {
                let (count, pages) = (r.u32()?, r.u32()?);
                ensure!(
                    count > 0 && pages > 0,
                    "paged bucket without records or pages"
                );
                ensure!(
                    pages <= count,
                    "paged bucket with {pages} pages for {count} records"
                );
                BucketBody::Paged {
                    count,
                    pages,
                    pages_ref: r.hash()?,
                }
            }
            p => bail!("bucket paged flag {p}"),
        };
        r.finish()?;
        Ok(Self {
            level,
            index,
            bucket,
            base,
            body,
        })
    }
}

/// A head feed update, or the origin (kind 6, 70 + 5·p bytes).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HeadRecord {
    /// `S`, the first unit the index covers (0: from genesis). The same in every head.
    /// Required in a saved state too: a state file from before this field belongs to an index
    /// in the earlier head format, which cannot be continued (see `State::load`).
    pub first_unit: u64,
    /// `N`, the number of complete units.
    pub units: u64,
    pub last_block_hash: B256,
    pub last_block_timestamp: u64,
    /// Deferred merges `(level, index)`, strictly ascending by `(index·2^level, level)`, the
    /// grid's first unit (not the one an index from a later unit holds from): an outer merge
    /// deferred while an inner one is still pending starts at the same unit.
    pub pending: Vec<(u8, u32)>,
}

/// Units are bounded by the format: spill pages store block numbers as u32, so at most
/// 2^32 / 64 = 2^26 units.
pub const MAX_UNITS: u64 = (1 << 32) / UNIT_BLOCKS;

/// The order of [`HeadRecord::pending`].
fn pending_key(slice: &(u8, u32)) -> (u64, u8) {
    (first_block(slice.0, slice.1), slice.0)
}

impl HeadRecord {
    pub fn first_block(&self) -> u64 {
        self.first_unit * UNIT_BLOCKS
    }

    pub fn last_block(&self) -> u64 {
        self.units * UNIT_BLOCKS - 1
    }

    pub fn encode(&self) -> Vec<u8> {
        assert!(
            (1..=MAX_UNITS).contains(&self.units),
            "head with {} units",
            self.units
        );
        assert!(
            self.first_unit < self.units,
            "head starting at unit {} with {} units",
            self.first_unit,
            self.units
        );
        assert!(
            self.pending
                .iter()
                .all(|(level, index)| ((*index as u64 + 1) << level) > self.first_unit),
            "a pending merge ends before the index starts"
        );
        assert!(
            self.pending
                .windows(2)
                .all(|w| pending_key(&w[0]) < pending_key(&w[1])),
            "pending merges not strictly ascending by (first unit, level)"
        );
        let mut w = Writer::new(KIND_HEAD);
        w.u64(self.first_block());
        w.u64(self.units);
        w.u64(self.last_block());
        w.hash(&self.last_block_hash);
        w.u64(self.last_block_timestamp);
        w.u8(u8::try_from(self.pending.len()).expect("more than 255 pending merges"));
        for (level, index) in &self.pending {
            w.u8(*level);
            w.u32(*index);
        }
        w.0
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf, KIND_HEAD)?;
        let first = r.u64()?;
        ensure!(
            first.is_multiple_of(UNIT_BLOCKS),
            "head starting at block {first}, not at a unit boundary"
        );
        let first_unit = first / UNIT_BLOCKS;
        let units = r.u64()?;
        ensure!(
            (1..=MAX_UNITS).contains(&units),
            "head with {units} units (1..={MAX_UNITS})"
        );
        ensure!(
            first_unit < units,
            "head starting at unit {first_unit} with {units} units"
        );
        let last = r.u64()?;
        ensure!(
            units.checked_mul(UNIT_BLOCKS).map(|b| b - 1) == Some(last),
            "head for {units} units states last block {last}"
        );
        let last_block_hash = r.hash()?;
        let last_block_timestamp = r.u64()?;
        let n = r.u8()?;
        let pending = (0..n)
            .map(|_| {
                let (level, index) = (r.u8()?, r.u32()?);
                // A deferred merge is at least level 1; a level-0 slice is never deferred.
                ensure!(
                    (1..=L_MAX).contains(&level),
                    "pending merge at level {level}"
                );
                Ok((level, index))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            pending
                .windows(2)
                .all(|w| pending_key(&w[0]) < pending_key(&w[1])),
            "pending merges not strictly ascending by (first unit, level)"
        );
        ensure!(
            pending
                .iter()
                .all(|(level, index)| ((*index as u64 + 1) << level) <= units),
            "a pending merge ends after unit {units}"
        );
        ensure!(
            pending
                .iter()
                .all(|(level, index)| ((*index as u64 + 1) << level) > first_unit),
            "a pending merge ends before the index starts at unit {first_unit}"
        );
        r.finish()?;
        Ok(Self {
            first_unit,
            units,
            last_block_hash,
            last_block_timestamp,
            pending,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::hex;

    #[test]
    fn varints() {
        for v in [
            0u64,
            1,
            127,
            128,
            300,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(out.len(), varint_len(v), "{v}");
            let mut s = &out[..];
            assert_eq!(get_varint(&mut s).unwrap(), v);
            assert!(s.is_empty());
        }
        // Not shortest: 1 as 0x81 0x00.
        assert!(get_varint(&mut &[0x81u8, 0x00][..]).is_err());
        // Wider than 64 bits.
        assert!(get_varint(&mut &[0xffu8; 10][..]).is_err());
        assert!(get_varint(&mut &[0x80u8][..]).is_err());
    }

    #[test]
    fn slice_geometry() {
        assert_eq!((first_block(0, 0), last_block(0, 0)), (0, 63));
        assert_eq!((first_block(3, 2), last_block(3, 2)), (1024, 1535));
        assert_eq!(
            (first_block(16, 6), last_block(16, 6)),
            (6 * 4_194_304, 7 * 4_194_304 - 1)
        );
    }

    fn e(block: u64, slot: u32, roles: u8) -> Entry {
        Entry { block, slot, roles }
    }

    #[test]
    fn records_roundtrip_and_sizes() {
        let s = SliceRecord {
            level: 3,
            index: 2,
            first_block: first_block(3, 2),
            last_block_hash: B256::repeat_byte(7),
            bucket_count: 5,
            addresses: 900,
            appearances: 2500,
            buckets_root: B256::repeat_byte(9),
            roles: 0xdf,
        };
        let enc = s.encode();
        assert_eq!(enc.len(), SliceRecord::LEN);
        assert_eq!(SliceRecord::decode(&enc).unwrap(), s);
        // A slice the index's start falls into begins at that unit.
        let held = SliceRecord {
            first_block: held_first_block(3, 2, 19),
            ..s.clone()
        };
        assert_eq!(held.first_block, 19 * UNIT_BLOCKS);
        assert_eq!(SliceRecord::decode(&held.encode()).unwrap(), held);

        let base = first_block(3, 2);
        let b = BucketRecord {
            level: 3,
            index: 2,
            bucket: 4,
            base,
            body: BucketBody::Inline(vec![
                AddressRecord {
                    fp: [1; 8],
                    entries: Entries::Inline(vec![
                        e(base, 1, 1),
                        e(base, 5, 3),
                        e(base + 70, 0, 0x40),
                    ]),
                },
                AddressRecord {
                    fp: [2; 8],
                    entries: Entries::Spilled {
                        count: 900,
                        spill_ref: B256::repeat_byte(3),
                    },
                },
            ]),
        };
        let enc = b.encode();
        let BucketBody::Inline(recs) = &b.body else {
            unreachable!()
        };
        assert_eq!(
            enc.len(),
            BucketRecord::INLINE_HEADER + recs.iter().map(|r| r.encoded_len(base)).sum::<usize>()
        );
        assert_eq!(BucketRecord::decode(&enc, base).unwrap(), b);
        // Deltas are relative to the slice's first block: the same bytes read from another
        // base are other blocks, or invalid.
        assert_ne!(BucketRecord::decode(&enc, base + 64).ok(), Some(b.clone()));

        let paged = BucketRecord {
            body: BucketBody::Paged {
                count: 400,
                pages: 2,
                pages_ref: B256::repeat_byte(4),
            },
            ..b
        };
        assert_eq!(BucketRecord::decode(&paged.encode(), base).unwrap(), paged);

        let h = HeadRecord {
            first_unit: 0,
            units: 412_950,
            last_block_hash: B256::repeat_byte(5),
            last_block_timestamp: 1_800_000_000,
            pending: vec![(14, 24), (8, 1612)],
        };
        let enc = h.encode();
        assert_eq!(enc.len(), 70 + 5 * 2);
        assert_eq!(HeadRecord::decode(&enc).unwrap(), h);
        let late = HeadRecord {
            first_unit: 390_000,
            ..h
        };
        assert_eq!(HeadRecord::decode(&late.encode()).unwrap(), late);
    }

    #[test]
    fn decoders_reject_non_canonical_input() {
        let base = 0;
        let rec = |entries| AddressRecord {
            fp: [0; 8],
            entries: Entries::Inline(entries),
        };
        let bucket = |records| BucketRecord {
            level: 0,
            index: 0,
            bucket: 0,
            base: 0,
            body: BucketBody::Inline(records),
        };
        // Entries out of order, duplicated, or without roles.
        for bad in [
            vec![e(5, 1, 1), e(3, 1, 1)],
            vec![e(5, 1, 1), e(5, 1, 2)],
            vec![e(5, 1, 0)],
        ] {
            let mut out = Vec::new();
            out.extend_from_slice(&[0; 8]);
            put_varint(&mut out, (bad.len() as u64) << 1);
            let mut prev = base;
            for x in &bad {
                put_varint(&mut out, x.block.saturating_sub(prev));
                put_varint(&mut out, x.slot as u64);
                out.push(x.roles);
                prev = x.block.max(prev);
            }
            assert!(
                AddressRecord::decode(&mut &out[..], base, 63).is_err(),
                "{bad:?}"
            );
        }
        // Trailing bytes.
        let mut enc = bucket(vec![rec(vec![e(1, 1, 1)])]).encode();
        enc.push(0);
        assert!(BucketRecord::decode(&enc, 0).is_err());
        // Records out of fingerprint order.
        let mut a_rec = rec(vec![e(1, 1, 1)]);
        a_rec.fp = [9; 8];
        assert!(
            BucketRecord::decode(&bucket(vec![a_rec, rec(vec![e(1, 1, 1)])]).encode(), 0).is_err()
        );
        // An inline record that should have been spilled.
        let long: Vec<Entry> = (0..100).map(|i| e(i * 200, 1, 1)).collect();
        assert!(entries_len(&long, 0) > SPILL_INLINE_MAX);
        assert!(BucketRecord::decode(&bucket(vec![rec(long)]).encode(), 0).is_err());
        // A spilled record with no entries; a paged bucket with no records.
        let spilled0 = AddressRecord {
            fp: [0; 8],
            entries: Entries::Spilled {
                count: 0,
                spill_ref: B256::ZERO,
            },
        };
        assert!(BucketRecord::decode(&bucket(vec![spilled0]).encode(), 0).is_err());
        let paged0 = BucketRecord {
            level: 0,
            index: 0,
            bucket: 0,
            base: 0,
            body: BucketBody::Paged {
                count: 0,
                pages: 1,
                pages_ref: B256::ZERO,
            },
        };
        assert!(BucketRecord::decode(&paged0.encode(), 0).is_err());
        // An inline bucket over one chunk (it should have been paged): canonical but for its
        // size, so only the size check can reject it.
        let many: Vec<AddressRecord> = (0..400u64)
            .map(|i| AddressRecord {
                fp: i.to_be_bytes(),
                entries: Entries::Inline(vec![e(1, 1, 1)]),
            })
            .collect();
        let big = bucket(many).encode();
        assert!(big.len() > PAGE_SIZE);
        assert!(BucketRecord::decode(&big, 0).is_err());
        // An entry after the slice's last block (slice (0, 0) ends at block 63).
        assert!(BucketRecord::decode(&bucket(vec![rec(vec![e(64, 1, 1)])]).encode(), 0).is_err());
        // A base outside the slice, or not at a unit boundary.
        let one = bucket(vec![rec(vec![e(1, 1, 1)])]).encode();
        assert!(BucketRecord::decode(&one, 64).is_err());
        assert!(BucketRecord::decode(&one, 1).is_err());
        // More pages than records.
        let pages_over = BucketRecord {
            level: 0,
            index: 0,
            bucket: 0,
            base: 0,
            body: BucketBody::Paged {
                count: 2,
                pages: 3,
                pages_ref: B256::ZERO,
            },
        };
        assert!(BucketRecord::decode(&pages_over.encode(), 0).is_err());
        // Units beyond the format's bound, and a pending merge that ends after the head.
        let head = |units, pending| HeadRecord {
            first_unit: 0,
            units,
            last_block_hash: B256::ZERO,
            last_block_timestamp: 0,
            pending,
        };
        let mut over = head(1, vec![]).encode();
        over[13..21].copy_from_slice(&(MAX_UNITS + 1).to_be_bytes());
        over[21..29].copy_from_slice(&((MAX_UNITS + 1) * UNIT_BLOCKS - 1).to_be_bytes());
        assert!(HeadRecord::decode(&over).is_err());
        assert!(HeadRecord::decode(&head(3, vec![(2, 0)]).encode()).is_err());
        // A start off a unit boundary, at or after the head's units, or after a pending merge.
        let start = |first: u64, units: u64, pending: Vec<(u8, u32)>| {
            let mut h = head(units, pending).encode();
            h[5..13].copy_from_slice(&first.to_be_bytes());
            h
        };
        assert!(HeadRecord::decode(&start(64 * 5, 10, vec![])).is_ok());
        assert!(HeadRecord::decode(&start(64 * 5 + 1, 10, vec![])).is_err());
        assert!(HeadRecord::decode(&start(64 * 10, 10, vec![])).is_err());
        assert!(HeadRecord::decode(&start(64 * 4, 10, vec![(1, 1)])).is_err());
        assert!(HeadRecord::decode(&start(64 * 3, 10, vec![(1, 1)])).is_ok());
        // A head with a level-0 or out-of-range pending merge.
        for level in [0u8, 17, 200] {
            let mut h = HeadRecord {
                first_unit: 0,
                units: 10,
                last_block_hash: B256::ZERO,
                last_block_timestamp: 0,
                pending: vec![(1, 0)],
            }
            .encode();
            let n = h.len();
            h[n - 5] = level;
            assert!(HeadRecord::decode(&h).is_err(), "level {level}");
        }
        // A slice record whose blocks do not match its level and index.
        let mut s = SliceRecord {
            level: 0,
            index: 1,
            first_block: 64,
            last_block_hash: B256::ZERO,
            bucket_count: 1,
            addresses: 0,
            appearances: 0,
            buckets_root: B256::ZERO,
            roles: 0,
        }
        .encode();
        s[21] ^= 1; // first_block, off a unit boundary
        assert!(SliceRecord::decode(&s).is_err());
        s[21] ^= 1;
        s[21] ^= 64; // first_block 0, before the slice
        assert!(SliceRecord::decode(&s).is_err());
    }

    /// An outer merge deferred while an inner one is pending starts at the same unit: the
    /// head lists both, ordered by (first unit, level).
    #[test]
    fn nested_pending_merges() {
        let h = HeadRecord {
            first_unit: 0,
            units: 4,
            last_block_hash: B256::ZERO,
            last_block_timestamp: 0,
            pending: vec![(1, 0), (2, 0)],
        };
        assert_eq!(HeadRecord::decode(&h.encode()).unwrap(), h);
        // The reverse order, or a duplicate, is not canonical.
        for pending in [vec![(2u8, 0u32), (1, 0)], vec![(1, 0), (1, 0)]] {
            let mut enc = h.encode();
            let n = enc.len();
            for (k, (level, index)) in pending.iter().enumerate() {
                let at = n - 10 + 5 * k;
                enc[at] = *level;
                enc[at + 1..at + 5].copy_from_slice(&index.to_be_bytes());
            }
            assert!(HeadRecord::decode(&enc).is_err(), "{pending:?}");
        }
    }

    #[test]
    #[should_panic(expected = "strictly ascending")]
    fn encoding_unordered_pending_panics() {
        HeadRecord {
            first_unit: 0,
            units: 4,
            last_block_hash: B256::ZERO,
            last_block_timestamp: 0,
            pending: vec![(2, 0), (1, 0)],
        }
        .encode();
    }

    /// Known answers: the exact bytes of each record kind, so that a change to encoder and
    /// decoder alike cannot silently change the format.
    #[test]
    fn known_bytes() {
        let base = first_block(1, 3);
        let bucket = BucketRecord {
            level: 1,
            index: 3,
            bucket: 2,
            base,
            body: BucketBody::Inline(vec![
                AddressRecord {
                    fp: [0x11; 8],
                    entries: Entries::Inline(vec![e(base, 1, 0x01), e(base + 130, 300, 0x18)]),
                },
                AddressRecord {
                    fp: [0x22; 8],
                    entries: Entries::Spilled {
                        count: 70,
                        spill_ref: B256::repeat_byte(0xab),
                    },
                },
            ]),
        };
        assert_eq!(hex::encode(bucket.encode()), BUCKET_HEX);
        assert_eq!(
            hex::encode(spill_file(&[
                e(5000, 2, 0x40),
                e(5000, 9, 1),
                e(5100, 0, 0x40)
            ])),
            SPILL_HEX
        );
        let slice = SliceRecord {
            level: 1,
            index: 3,
            first_block: base,
            last_block_hash: B256::repeat_byte(0xcd),
            bucket_count: 3,
            addresses: 2,
            appearances: 72,
            buckets_root: B256::repeat_byte(0xef),
            roles: 0xdf,
        };
        assert_eq!(hex::encode(slice.encode()), SLICE_HEX);
        let head = HeadRecord {
            first_unit: 0,
            units: 8,
            last_block_hash: B256::repeat_byte(0x01),
            last_block_timestamp: 0x6000_0000,
            pending: vec![(2, 1)],
        };
        assert_eq!(hex::encode(head.encode()), HEAD_HEX);
        // An index from unit 7 (block 448): the start in the head, the slice the start falls
        // into from there, and its entries' deltas from that block.
        let late = HeadRecord {
            first_unit: 7,
            ..head
        };
        assert_eq!(hex::encode(late.encode()), HEAD_LATE_HEX);
        let held = SliceRecord {
            first_block: held_first_block(1, 3, 7),
            ..slice
        };
        assert_eq!(hex::encode(held.encode()), SLICE_LATE_HEX);
        let held_bucket = BucketRecord {
            base: held.first_block,
            body: BucketBody::Inline(vec![AddressRecord {
                fp: [0x11; 8],
                entries: Entries::Inline(vec![e(450, 1, 0x01), e(500, 300, 0x18)]),
            }]),
            ..bucket
        };
        assert_eq!(hex::encode(held_bucket.encode()), BUCKET_LATE_HEX);
    }

    const BUCKET_HEX: &str = "453253010501000000030000000200000000021111111111111111040001018201ac021822222222222222228d01abababababababababababababababababababababababababababababababab";
    const SPILL_HEX: &str = "000300001388000240000901640040";
    const SLICE_HEX: &str = "4532530104000000400100000003000000000000018000000000000001ffcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd00000003000000020000000000000048efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefdf";
    const HEAD_LATE_HEX: &str = "453253010600000000000001c0000000000000000800000000000001ff01010101010101010101010101010101010101010101010101010101010101010000000060000000010200000001";
    const SLICE_LATE_HEX: &str = "453253010400000040010000000300000000000001c000000000000001ffcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd00000003000000020000000000000048efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefdf";
    const BUCKET_LATE_HEX: &str =
        "4532530105010000000300000002000000000111111111111111110402010132ac0218";
    const HEAD_HEX: &str = "45325301060000000000000000000000000000000800000000000001ff01010101010101010101010101010101010101010101010101010101010101010000000060000000010200000001";

    #[test]
    fn spill_pages() {
        // Enough entries for three pages; deltas of 1, so entries are 3 bytes.
        let entries: Vec<Entry> = (0..3000).map(|i| e(1000 + i, 1, 1)).collect();
        let file = spill_file(&entries);
        let pages = pages(&file).unwrap();
        assert_eq!(pages.len(), 3);
        assert!(pages[..2].iter().all(|p| p.len() == PAGE_SIZE));
        let decoded: Vec<Entry> = pages
            .iter()
            .flat_map(|p| decode_spill_page(p, 0, 4095).unwrap())
            .collect();
        assert_eq!(decoded, entries);
        // Greedy: (4096 − 6) / 3 entries on a full page.
        assert_eq!(decode_spill_page(pages[0], 0, 4095).unwrap().len(), 1363);
        // The last page has no padding.
        assert!(*file.last().unwrap() != 0);
        // Non-zero bytes in the padding are rejected.
        let mut bad = pages[0].to_vec();
        *bad.last_mut().unwrap() = 1;
        assert!(decode_spill_page(&bad, 0, 4095).is_err());
        // Pages of another slice: entries after its last block, or a start before its first.
        assert!(decode_spill_page(pages[2], 0, 3000).is_err());
        assert!(decode_spill_page(pages[0], 1001, 4095).is_err());
    }

    #[test]
    fn bucket_pages() {
        let base = 0;
        let records: Vec<AddressRecord> = (0..600u32)
            .map(|i| AddressRecord {
                fp: (i as u64).to_be_bytes(),
                entries: Entries::Inline(vec![e(i as u64, 2, 1)]),
            })
            .collect();
        let file = bucket_pages_file(&records, base);
        let decoded: Vec<AddressRecord> = pages(&file)
            .unwrap()
            .iter()
            .flat_map(|p| decode_bucket_page(p, base, 1023).unwrap())
            .collect();
        assert_eq!(decoded, records);
        assert!(file.len() > PAGE_SIZE);
    }

    #[test]
    fn identifiers() {
        assert_eq!(slice_id(1, 16, 6), keccak256("eth:1:addr1:slice:16:6"));
        assert_eq!(
            bucket_id(1337, 0, 12, 3),
            keccak256("eth:1337:addr1:bucket:0:12:3")
        );
        let mut buf = keccak256("eth:1:addr1:head").to_vec();
        buf.extend(5u64.to_be_bytes());
        assert_eq!(head_id(1, 5), keccak256(buf));
        assert_ne!(origin_id(1), head_id(1, 0));
    }

    #[test]
    fn a_spill_file_checked_page_by_page_accepts_what_a_whole_read_does() {
        let entries: Vec<Entry> = (0..3000u64)
            .map(|i| Entry {
                block: 1000 + i,
                slot: 1,
                roles: 1,
            })
            .collect();
        let file = spill_file(&entries);
        let pages: Vec<&[u8]> = file.chunks(PAGE_SIZE).collect();
        assert_eq!(pages.len(), 3);
        let check = |pages: &[&[u8]], count: u64, span: usize| -> Result<Vec<Entry>> {
            let mut c = SpillPageCheck::new(0, 1 << 20, count, span as u64)?;
            let mut out = Vec::new();
            for p in pages {
                out.extend(c.page(p)?);
            }
            ensure!(c.done(), "pages missing");
            Ok(out)
        };
        assert_eq!(check(&pages, 3000, file.len()).unwrap(), entries);
        let refused = |pages: &[&[u8]], count: u64, span: usize, why: &str| {
            let err = format!("{:#}", check(pages, count, span).unwrap_err());
            assert!(err.contains(why), "expected {why:?}, got {err:?}");
        };
        refused(
            &[pages[1], pages[0], pages[2]],
            3000,
            file.len(),
            "not ascending",
        );
        refused(&pages, 2999, file.len(), "more than the 2999");
        refused(&pages, 3001, file.len(), "the record states 3001");
        refused(&pages[..2], 3000, file.len(), "pages missing");
        // A page cut one entry early, the rest moved on: not the writer's greedy fill.
        let n0 = decode_spill_page(pages[0], 0, 1 << 20).unwrap().len();
        let mut short = spill_file(&entries[..n0 - 1]);
        short.resize(PAGE_SIZE, 0);
        let rest = spill_file(&entries[n0 - 1..]);
        let refit: Vec<&[u8]> = std::iter::once(&short[..])
            .chain(rest.chunks(PAGE_SIZE))
            .collect();
        refused(&refit, 3000, PAGE_SIZE + rest.len(), "page 0 ends early");
        // A padded last page, and a last page the span disagrees with.
        let mut padded = pages[2].to_vec();
        padded.resize(PAGE_SIZE, 0);
        refused(
            &[pages[0], pages[1], &padded],
            3000,
            2 * PAGE_SIZE + PAGE_SIZE,
            "padded",
        );
        refused(&pages, 3000, file.len() + 1, "the span says");
        // A failed page leaves the check as it was.
        let mut c = SpillPageCheck::new(0, 1 << 20, 3000, file.len() as u64).unwrap();
        c.page(pages[0]).unwrap();
        assert!(c.page(pages[2]).is_err());
        c.page(pages[1]).unwrap();
        c.page(pages[2]).unwrap();
        assert!(c.done());
    }
}
