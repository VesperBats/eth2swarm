//! `TRACE` from an archive node's own index (#8, part 12): A1TR files written by
//! `tools/erigon-trace-export` from Erigon's `tracesfrom`/`tracesto`, read here in place of
//! `trace_block`.
//!
//! A1TR v2 (the writer's `a1tr.go` is the reference):
//!
//! ```text
//! header:      "A1TR" ‖ version u8 = 2 ‖ chain_id u64 BE ‖ first_block u64 BE ‖
//!              last_block u64 BE ‖ record_count u64 BE ‖ source_len u8 ‖ source
//! block table: one entry per block first..=last: hash 32 ‖ tx_count u32 BE
//! record:      block u64 BE ‖ tx_index u32 BE ‖ address 20   (32 bytes)
//! ```
//!
//! Records are sorted by (block, tx_index, address) without duplicates. Every file is bound to
//! the chain the importer reads: its chain ID, and for every block the hash and transaction
//! count, which [`TraceIndex::block`]'s caller compares with the block it fetched. A block no
//! file covers is an error: an index with a hole must not import as "no traces".

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256};
use anyhow::{Context, Result, bail, ensure};

const MAGIC: &[u8; 4] = b"A1TR";
const VERSION: u8 = 2;
const FIXED_HEADER: u64 = 4 + 1 + 8 + 8 + 8 + 8 + 1;
const BLOCK_ENTRY: u64 = 32 + 4;
const RECORD: u64 = 8 + 4 + 20;

/// One A1TR file, opened and checked.
#[derive(Debug)]
struct IndexFile {
    path: PathBuf,
    file: File,
    first: u64,
    last: u64,
    records: u64,
    /// Offset of the block table.
    table_at: u64,
    /// Offset of the first record.
    records_at: u64,
}

/// What the index holds for one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedBlock {
    pub hash: B256,
    pub tx_count: u32,
    /// (transaction index, address), ascending.
    pub frames: Vec<(u32, Address)>,
}

/// A directory of A1TR files for one chain.
#[derive(Debug)]
pub struct TraceIndex {
    files: Vec<IndexFile>,
}

impl TraceIndex {
    /// Opens every `trace-*.a1tr` file in `dir`, checking its header and size, its chain ID
    /// against `chain_id`, and that no two files overlap.
    pub fn open(dir: &Path, chain_id: u64) -> Result<Self> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !(name.starts_with("trace-") && name.ends_with(".a1tr")) {
                continue;
            }
            files.push(IndexFile::open(&path, chain_id)?);
        }
        ensure!(
            !files.is_empty(),
            "no trace-*.a1tr file in {}",
            dir.display()
        );
        files.sort_by_key(|f| f.first);
        for w in files.windows(2) {
            ensure!(
                w[0].last < w[1].first,
                "{} and {} overlap",
                w[0].path.display(),
                w[1].path.display()
            );
        }
        Ok(Self { files })
    }

    /// The blocks the files cover, as ranges (gaps between files are not covered).
    pub fn ranges(&self) -> Vec<std::ops::RangeInclusive<u64>> {
        self.files.iter().map(|f| f.first..=f.last).collect()
    }

    /// The last block of the coverage that starts at `start` and has no gap (files that
    /// adjoin count as one); `None` if no file covers `start`.
    pub fn covered_until(&self, start: u64) -> Option<u64> {
        let mut i = self
            .files
            .iter()
            .position(|f| (f.first..=f.last).contains(&start))?;
        let mut end = self.files[i].last;
        while self
            .files
            .get(i + 1)
            .is_some_and(|f| Some(f.first) == end.checked_add(1))
        {
            i += 1;
            end = self.files[i].last;
        }
        Some(end)
    }

    /// The index's entry for block `n`; an error if no file covers it.
    pub fn block(&self, n: u64) -> Result<IndexedBlock> {
        let Some(f) = self.files.iter().find(|f| (f.first..=f.last).contains(&n)) else {
            bail!("no trace index file covers block {n}");
        };
        f.block(n)
    }
}

impl IndexFile {
    fn open(path: &Path, chain_id: u64) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let len = file.metadata()?.len();
        let mut head = [0u8; FIXED_HEADER as usize];
        file.read_exact_at(&mut head, 0)
            .with_context(|| format!("{}: header truncated", path.display()))?;
        let u64_at = |i: usize| u64::from_be_bytes(head[i..i + 8].try_into().unwrap());
        ensure!(
            &head[..4] == MAGIC,
            "{} is not an A1TR file",
            path.display()
        );
        ensure!(
            head[4] == VERSION,
            "{} is A1TR version {}, this build reads {VERSION}",
            path.display(),
            head[4]
        );
        let (chain, first, last, records) = (u64_at(5), u64_at(13), u64_at(21), u64_at(29));
        ensure!(
            chain == chain_id,
            "{} is for chain {chain}, the import for chain {chain_id}",
            path.display()
        );
        ensure!(
            first <= last,
            "{}: block range {first}..={last}",
            path.display()
        );
        let source_len = head[37] as u64;
        let table_at = FIXED_HEADER + source_len;
        // Checked: a damaged header must not wrap into a size that happens to match.
        let size = (last - first)
            .checked_add(1)
            .and_then(|blocks| blocks.checked_mul(BLOCK_ENTRY))
            .and_then(|t| t.checked_add(table_at))
            .and_then(|records_at| {
                records
                    .checked_mul(RECORD)
                    .and_then(|r| r.checked_add(records_at))
                    .map(|end| (records_at, end))
            });
        let Some((records_at, end)) = size else {
            bail!("{}: a header whose sizes overflow", path.display());
        };
        ensure!(
            len == end,
            "{} has {len} bytes, its header says {end} (an incomplete or damaged file)",
            path.display()
        );
        Ok(Self {
            path: path.to_path_buf(),
            file,
            first,
            last,
            records,
            table_at,
            records_at,
        })
    }

    fn record(&self, i: u64) -> Result<(u64, u32, Address)> {
        let mut r = [0u8; RECORD as usize];
        self.file
            .read_exact_at(&mut r, self.records_at + i * RECORD)?;
        Ok((
            u64::from_be_bytes(r[..8].try_into().unwrap()),
            u32::from_be_bytes(r[8..12].try_into().unwrap()),
            Address::from_slice(&r[12..]),
        ))
    }

    /// The first record at or after block `n` (binary search: records are sorted by block).
    fn lower_bound(&self, n: u64) -> Result<u64> {
        let (mut lo, mut hi) = (0, self.records);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.record(mid)?.0 < n {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    fn block(&self, n: u64) -> Result<IndexedBlock> {
        let mut entry = [0u8; BLOCK_ENTRY as usize];
        self.file
            .read_exact_at(&mut entry, self.table_at + (n - self.first) * BLOCK_ENTRY)?;
        let hash = B256::from_slice(&entry[..32]);
        let tx_count = u32::from_be_bytes(entry[32..].try_into().unwrap());
        let mut frames: Vec<(u32, Address)> = Vec::new();
        let mut i = self.lower_bound(n)?;
        while i < self.records {
            let (block, tx, addr) = self.record(i)?;
            if block != n {
                // The binary search trusts the order: a smaller block here means it is broken,
                // and frames of this block may lie beyond.
                ensure!(
                    block > n,
                    "{}: record {i} is for block {block} after records for block {n} (records \
                     not ascending)",
                    self.path.display()
                );
                break;
            }
            ensure!(
                tx < tx_count,
                "{}: block {n} has {tx_count} transactions, a record names transaction {tx}",
                self.path.display()
            );
            ensure!(
                frames.last().is_none_or(|p| *p < (tx, addr)),
                "{}: block {n}: records not ascending",
                self.path.display()
            );
            frames.push((tx, addr));
            i += 1;
        }
        Ok(IndexedBlock {
            hash,
            tx_count,
            frames,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Writes an A1TR v2 file as `tools/erigon-trace-export` does.
    pub(crate) fn write(
        dir: &Path,
        chain: u64,
        first: u64,
        last: u64,
        blocks: &[(B256, u32)],
        records: &[(u64, u32, Address)],
    ) -> PathBuf {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        for v in [chain, first, last, records.len() as u64] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.push(1);
        out.push(b'x');
        for (h, n) in blocks {
            out.extend_from_slice(h.as_slice());
            out.extend_from_slice(&n.to_be_bytes());
        }
        for (b, t, a) in records {
            out.extend_from_slice(&b.to_be_bytes());
            out.extend_from_slice(&t.to_be_bytes());
            out.extend_from_slice(a.as_slice());
        }
        let path = dir.join(format!("trace-{first:010}-{last:010}.a1tr"));
        std::fs::write(&path, out).unwrap();
        path
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("a1tr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The bytes `tools/erigon-trace-export`'s `TestA1TRLayout` requires of the writer: blocks
    /// 1..=2 of chain 1 with hashes 0x01…/0x02… and 6 transactions each, source "x", and one
    /// record (block 2, transaction 5, 0xaa00…07).
    const GOLDEN: &str = include_str!("../../tools/erigon-trace-export/testdata/a1tr-layout.hex");

    fn golden() -> Vec<u8> {
        let hex: String = GOLDEN.split_whitespace().collect();
        alloy::primitives::hex::decode(hex).unwrap()
    }

    fn golden_hash(first: u8) -> B256 {
        let mut h = B256::ZERO;
        h.0[0] = first;
        h
    }

    fn golden_address() -> Address {
        let mut a = Address::ZERO;
        a.0[0] = 0xaa;
        a.0[19] = 7;
        a
    }

    #[test]
    fn the_writers_layout_reads_back() {
        let dir = tmp("layout");
        std::fs::write(dir.join("trace-0000000001-0000000002.a1tr"), golden()).unwrap();
        let idx = TraceIndex::open(&dir, 1).unwrap();
        assert_eq!(idx.ranges(), vec![1..=2]);
        assert_eq!(
            idx.block(1).unwrap(),
            IndexedBlock {
                hash: golden_hash(1),
                tx_count: 6,
                frames: vec![]
            }
        );
        assert_eq!(
            idx.block(2).unwrap(),
            IndexedBlock {
                hash: golden_hash(2),
                tx_count: 6,
                frames: vec![(5, golden_address())]
            }
        );
        assert!(idx.block(3).is_err(), "no file covers block 3");
        // Another chain is refused.
        assert!(TraceIndex::open(&dir, 5).is_err());
        // The test writer produces the same bytes.
        let d2 = tmp("layout-writer");
        let p = write(
            &d2,
            1,
            1,
            2,
            &[(golden_hash(1), 6), (golden_hash(2), 6)],
            &[(2, 5, golden_address())],
        );
        assert_eq!(std::fs::read(p).unwrap(), golden());
        for d in [dir, d2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn damaged_headers_and_directories_are_refused() {
        let open = |name: &str, bytes: &[u8]| {
            let d = tmp(name);
            std::fs::write(d.join("trace-0000000001-0000000002.a1tr"), bytes).unwrap();
            let r = TraceIndex::open(&d, 1)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"));
            let _ = std::fs::remove_dir_all(d);
            r
        };
        assert_eq!(open("ok", &golden()), Ok(()));
        let patched = |at: usize, with: &[u8]| {
            let mut b = golden();
            b[at..at + with.len()].copy_from_slice(with);
            b
        };
        let err = |name: &str, bytes: Vec<u8>, says: &str| {
            let e = open(name, &bytes).unwrap_err();
            assert!(e.contains(says), "{name}: {e}");
        };
        err("magic", patched(0, b"A1TX"), "not an A1TR file");
        err("version", patched(4, &[3]), "version 3");
        // first 3 > last 2.
        err(
            "range",
            patched(13, &3u64.to_be_bytes()),
            "block range 3..=2",
        );
        // A record count whose size overflows, and a last block whose table does.
        err("records", patched(29, &u64::MAX.to_be_bytes()), "overflow");
        err("table", patched(21, &u64::MAX.to_be_bytes()), "overflow");
        err("short header", golden()[..20].to_vec(), "header truncated");
        // No file at all, and only files under other names.
        let empty = tmp("empty");
        std::fs::write(empty.join("notes.txt"), b"x").unwrap();
        let e = TraceIndex::open(&empty, 1).unwrap_err();
        assert!(format!("{e:#}").contains("no trace-*.a1tr file"), "{e:#}");
        let _ = std::fs::remove_dir_all(empty);
    }

    #[test]
    fn records_out_of_order_are_refused() {
        // A record of block 1 after one of block 2: the binary search for block 2 lands on
        // its record and must not stop quietly at the stray one.
        let dir = tmp("order");
        let a = Address::repeat_byte(9);
        write(
            &dir,
            1,
            1,
            2,
            &[(B256::ZERO, 2); 2],
            &[(1, 0, a), (2, 0, a), (1, 1, a)],
        );
        let idx = TraceIndex::open(&dir, 1).unwrap();
        let e = idx.block(2).unwrap_err();
        assert!(format!("{e:#}").contains("not ascending"), "{e:#}");
        // Within a block, too.
        let d2 = tmp("order-tx");
        write(&d2, 1, 1, 1, &[(B256::ZERO, 2)], &[(1, 1, a), (1, 0, a)]);
        let e = TraceIndex::open(&d2, 1).unwrap().block(1).unwrap_err();
        assert!(format!("{e:#}").contains("not ascending"), "{e:#}");
        for d in [dir, d2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn coverage_runs_over_adjoining_files() {
        let dir = tmp("coverage");
        write(&dir, 1, 10, 19, &[(B256::ZERO, 0); 10], &[]);
        write(&dir, 1, 20, 29, &[(B256::ZERO, 0); 10], &[]);
        write(&dir, 1, 40, 49, &[(B256::ZERO, 0); 10], &[]);
        let idx = TraceIndex::open(&dir, 1).unwrap();
        assert_eq!(idx.covered_until(10), Some(29));
        assert_eq!(idx.covered_until(25), Some(29));
        assert_eq!(idx.covered_until(30), None);
        assert_eq!(idx.covered_until(9), None);
        assert_eq!(idx.covered_until(45), Some(49));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn records_are_found_by_block_and_damage_is_refused() {
        let dir = tmp("search");
        let blocks: Vec<(B256, u32)> = (0..100).map(|i| (B256::repeat_byte(i as u8), 10)).collect();
        let records: Vec<(u64, u32, Address)> = (0..100u64)
            .filter(|b| b % 3 != 0)
            .flat_map(|b| {
                (0..(b % 4) as u32).map(move |t| (1000 + b, t, Address::repeat_byte(t as u8 + 1)))
            })
            .collect();
        write(&dir, 1, 1000, 1099, &blocks, &records);
        let idx = TraceIndex::open(&dir, 1).unwrap();
        for b in 0..100u64 {
            let want: Vec<(u32, Address)> = records
                .iter()
                .filter(|r| r.0 == 1000 + b)
                .map(|r| (r.1, r.2))
                .collect();
            assert_eq!(
                idx.block(1000 + b).unwrap().frames,
                want,
                "block {}",
                1000 + b
            );
        }
        // A truncated file, an overlapping file, a record beyond its block's transactions.
        let d2 = tmp("damage");
        let p = write(
            &d2,
            1,
            1,
            2,
            &[(B256::ZERO, 1), (B256::ZERO, 1)],
            &[(1, 0, Address::ZERO)],
        );
        let data = std::fs::read(&p).unwrap();
        std::fs::write(&p, &data[..data.len() - 1]).unwrap();
        assert!(TraceIndex::open(&d2, 1).is_err());
        let d3 = tmp("overlap");
        write(&d3, 1, 1, 5, &[(B256::ZERO, 1); 5], &[]);
        write(&d3, 1, 5, 6, &[(B256::ZERO, 1); 2], &[]);
        assert!(TraceIndex::open(&d3, 1).is_err());
        let d4 = tmp("tx");
        write(&d4, 1, 1, 1, &[(B256::ZERO, 2)], &[(1, 2, Address::ZERO)]);
        assert!(TraceIndex::open(&d4, 1).unwrap().block(1).is_err());
        for d in [dir, d2, d3, d4] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
