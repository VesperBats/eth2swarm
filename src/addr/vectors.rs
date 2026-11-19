//! Reader conformance vectors: slices built by the slice builder at the edges of the format
//! (spill files and pages files of several pages, pages filled to the last byte, an address at
//! the inline limit and one byte over, a bucket of exactly one chunk and one byte over, which
//! is paged, a spilled record in a paged bucket, a slice of an index that starts inside it, a
//! slice at mainnet height), written to `testdata/addr1-reader-vectors.txt` for independent
//! readers (Myotis' `myotis-addr1`) to decode and compare. Everything the file states is read
//! back from the bytes it lists with this crate's decoders before it is written, and the test
//! fails when the builder's output changes: a change of the format then shows up as a diff of
//! that file. Regenerate with `ETH2SWARM_WRITE_VECTORS=1 cargo test --locked addr::vectors`.

use std::collections::HashMap;
use std::fmt::Write as _;

use alloy::hex;
use alloy::primitives::{Address, B256, keccak256};

use super::detect::{Appearance, ignored, role};
use super::layout::{
    AddressRecord, BucketBody, BucketRecord, Entries, Entry, PAGE_SIZE, SPILL_INLINE_MAX,
    SliceRecord, decode_bucket_page, decode_spill_page, entries_len, first_block, held_first_block,
    pages, varint_len,
};
use super::slice::{SliceId, bucket_of, build_slice, fingerprint};
use crate::swarm::file::file_reference;

const PATH: &str = "testdata/addr1-reader-vectors.txt";

/// What a case is the edge of, checked after it is built.
enum Edge {
    /// One spill file of this many pages.
    SpillPages(usize),
    /// One spill file whose first page is filled to its last byte (no padding), then a second.
    SpillPageFull,
    /// The first address's inline entries are exactly the inline limit, the second's one byte
    /// more.
    SpillLimit,
    /// The addresses fill one bucket of exactly this many bytes inline (paged above
    /// [`PAGE_SIZE`]), its pages file of this many pages if paged.
    Bucket(usize, usize),
    /// A paged bucket whose first page is filled to its last byte (no padding).
    BucketPageFull,
    /// An index that starts inside the slice: entries are relative to the held first block.
    LateStart,
    /// Block numbers above 2^24 and block deltas of three and four bytes.
    MainnetHeight,
}

/// One case: a slice and the addresses it holds with their entries.
struct Case {
    name: &'static str,
    about: &'static str,
    slice: SliceId,
    first_unit: u64,
    addresses: Vec<(Address, Vec<Entry>)>,
    edge: Edge,
}

fn entry(block: u64, slot: u32) -> Entry {
    Entry {
        block,
        slot,
        roles: role::FROM,
    }
}

/// `n` entries of 3 bytes each at block `base` (slots 1..=n), then `wide` of 4 bytes (slots
/// 200, 201, …: a two-byte varint).
fn entries(base: u64, n: u32, wide: u32) -> Vec<Entry> {
    (1..=n)
        .map(|s| entry(base, s))
        .chain((0..wide).map(|w| entry(base, 200 + w)))
        .collect()
}

/// An address of the vectors: above `0x…ffff`, which detection never indexes.
fn addr(i: u64) -> Address {
    Address::left_padding_from(&(0xa000_0000 + i).to_be_bytes())
}

/// Addresses (above `0x…ffff`) whose hash starts below 0x55: bucket 0 of a slice of two or
/// three buckets.
fn low_addresses() -> impl Iterator<Item = Address> {
    (0x1_0000u64..)
        .map(|i| Address::left_padding_from(&i.to_be_bytes()))
        .filter(|a| keccak256(a.as_slice())[0] < 0x55)
}

/// Bytes of the record of `e` in the form the builder stores it (inline, or spilled).
fn record_len(e: &[Entry], base: u64) -> usize {
    let n = e.len() as u64;
    if entries_len(e, base) > SPILL_INLINE_MAX {
        8 + varint_len((n << 1) | 1) + 32
    } else {
        8 + varint_len(n << 1) + entries_len(e, base)
    }
}

/// A bucket of `n - 1` addresses of 40 entries and one of `last` (3-byte entries, plus `wide`
/// 4-byte ones) at block `base`.
fn full_bucket(base: u64, n: usize, last: u32, wide: u32) -> Vec<(Address, Vec<Entry>)> {
    low_addresses()
        .take(n)
        .enumerate()
        .map(|(i, a)| {
            let e = if i + 1 < n {
                entries(base, 40, 0)
            } else {
                entries(base, last, wide)
            };
            (a, e)
        })
        .collect()
}

fn cases() -> Vec<Case> {
    let base = |index: u32| held_first_block(0, index, 0);
    // A bucket whose first page holds exactly 4096 bytes: records go into pages in
    // fingerprint order, so the 32nd smallest fingerprint gets a record of 95 bytes and the
    // others 129 (2 + 31 · 129 + 95 = 4096); ten more records go to the second page.
    let page_full = {
        let mut a: Vec<Address> = low_addresses().take(42).collect();
        a.sort_by_key(|a| fingerprint(&keccak256(a.as_slice())));
        a.into_iter()
            .enumerate()
            .map(|(i, a)| {
                let e = if i == 31 {
                    entries(base(7), 26, 2)
                } else {
                    entries(base(7), 40, 0)
                };
                (a, e)
            })
            .collect()
    };
    let mut bucket_pages = full_bucket(base(5), 63, 40, 0);
    bucket_pages.push((addr(6), entries(base(5), 100, 0)));
    let bucket_pages_len = BucketRecord::INLINE_HEADER
        + bucket_pages
            .iter()
            .map(|(_, e)| record_len(e, base(5)))
            .sum::<usize>();
    let high = held_first_block(16, 6, 0);
    vec![
        Case {
            name: "spill-pages",
            about: "one address with 2000 entries: a spill file of two pages",
            slice: (0, 1),
            first_unit: 0,
            addresses: vec![(
                addr(1),
                (0..2000u32)
                    .map(|j| entry(base(1) + (j / 32) as u64, j % 32 + 1))
                    .collect(),
            )],
            edge: Edge::SpillPages(2),
        },
        Case {
            name: "spill-page-full",
            about: "a spill page filled to its last byte (6 + 1362 · 3 + 4), then a second page",
            slice: (0, 6),
            first_unit: 0,
            addresses: vec![(addr(2), {
                let mut e: Vec<Entry> = (0..1362u32)
                    .map(|j| entry(base(6) + (j / 32) as u64, j % 32 + 1))
                    .collect();
                e.push(entry(base(6) + 1361 / 32, 200));
                e.extend((1..=10).map(|s| entry(base(6) + 43, s)));
                e
            })],
            edge: Edge::SpillPageFull,
        },
        Case {
            name: "spill-limit",
            about: "entries of exactly 256 bytes stay inline, 257 bytes spill",
            slice: (0, 2),
            first_unit: 0,
            addresses: vec![
                (addr(3), entries(base(2), 84, 1)),
                (addr(4), entries(base(2), 83, 2)),
            ],
            edge: Edge::SpillLimit,
        },
        Case {
            name: "bucket-4096",
            about: "a bucket of exactly 4096 bytes inline",
            slice: (0, 3),
            first_unit: 0,
            addresses: full_bucket(base(3), 32, 23, 0),
            edge: Edge::Bucket(PAGE_SIZE, 0),
        },
        Case {
            name: "bucket-4097",
            about: "a bucket one byte over 4096: paged, its pages file one page",
            slice: (0, 4),
            first_unit: 0,
            addresses: full_bucket(base(4), 32, 22, 1),
            edge: Edge::Bucket(PAGE_SIZE + 1, 1),
        },
        Case {
            name: "bucket-pages",
            about: "a bucket of 64 addresses, one of them spilled: paged, its pages file three pages",
            slice: (0, 5),
            first_unit: 0,
            addresses: bucket_pages,
            edge: Edge::Bucket(bucket_pages_len, 3),
        },
        Case {
            name: "bucket-page-full",
            about: "a paged bucket whose first page is filled to its last byte (2 + 31 · 129 + 95)",
            slice: (0, 7),
            first_unit: 0,
            addresses: page_full,
            edge: Edge::BucketPageFull,
        },
        Case {
            name: "late-start",
            about: "slice (2, 0) of an index from unit 3 (block 192), roles of several bits, \
                    one address spilled",
            slice: (2, 0),
            first_unit: 3,
            addresses: vec![
                (
                    addr(5),
                    vec![
                        Entry {
                            block: 192,
                            slot: 0,
                            roles: role::BLOCK,
                        },
                        Entry {
                            block: 200,
                            slot: 3,
                            roles: role::FROM | role::EMITTER,
                        },
                        Entry {
                            block: 255,
                            slot: 1,
                            roles: role::AUTH | role::TO | role::WORD,
                        },
                    ],
                ),
                (
                    addr(7),
                    (0..128u32)
                        .map(|j| Entry {
                            block: 192 + (j / 2) as u64,
                            slot: j % 2 + 1,
                            roles: [role::FROM, role::TOPIC | role::TO, role::WORD][j as usize % 3],
                        })
                        .collect(),
                ),
            ],
            edge: Edge::LateStart,
        },
        Case {
            name: "mainnet-height",
            about: "slice (16, 6) from block 25165824: block deltas of three and four bytes, \
                    one address spilled",
            slice: (16, 6),
            first_unit: 0,
            addresses: vec![
                (
                    addr(8),
                    [0, 100, 20_100, 4_020_100]
                        .iter()
                        .enumerate()
                        .map(|(i, d)| Entry {
                            block: high + d,
                            slot: 300 * i as u32,
                            roles: role::FROM | role::TO,
                        })
                        .collect(),
                ),
                (
                    addr(9),
                    (0..100u64)
                        .map(|j| Entry {
                            block: high + 40_000 * j,
                            slot: (j % 3) as u32,
                            roles: role::TOPIC,
                        })
                        .collect(),
                ),
            ],
            edge: Edge::MainnetHeight,
        },
    ]
}

fn line_entries(e: &[Entry]) -> String {
    e.iter()
        .map(|e| format!("{}.{}.{}", e.block, e.slot, e.roles))
        .collect::<Vec<_>>()
        .join(",")
}

/// The records of a bucket payload, from a paged bucket's pages file too, checked against
/// the counts the bucket states; with whether it is paged.
fn bucket_records(
    rec: &SliceRecord,
    payload: &[u8],
    k: u32,
    files: &HashMap<B256, Vec<u8>>,
) -> (bool, Vec<AddressRecord>) {
    let (first, last) = (rec.first_block, rec.last_block());
    let b = BucketRecord::decode(payload, first).unwrap();
    assert_eq!((b.level, b.index, b.bucket), (rec.level, rec.index, k));
    match b.body {
        BucketBody::Inline(r) => (false, r),
        BucketBody::Paged {
            count,
            pages: n,
            pages_ref,
        } => {
            let split = pages(&files[&pages_ref]).unwrap();
            assert_eq!(split.len(), n as usize, "the bucket's page count");
            let r: Vec<AddressRecord> = split
                .into_iter()
                .flat_map(|p| decode_bucket_page(p, first, last).unwrap())
                .collect();
            assert_eq!(r.len(), count as usize, "the bucket's record count");
            (true, r)
        }
    }
}

/// What the written bytes hold for `address`, read back with this crate's decoders: whether
/// its bucket is paged, whether its record spilled, and its entries (`None`: no record).
fn read_back(
    rec: &SliceRecord,
    buckets: &[Vec<u8>],
    files: &HashMap<B256, Vec<u8>>,
    address: &Address,
) -> (bool, bool, Option<Vec<Entry>>) {
    let (first, last) = (rec.first_block, rec.last_block());
    let h = keccak256(address.as_slice());
    let k = bucket_of(&h, rec.bucket_count);
    let (paged, records) = bucket_records(rec, &buckets[k as usize], k, files);
    let mine: Vec<&AddressRecord> = records.iter().filter(|r| r.fp == fingerprint(&h)).collect();
    assert!(mine.len() <= 1, "two records with one fingerprint");
    let Some(r) = mine.first() else {
        return (paged, false, None);
    };
    match &r.entries {
        Entries::Inline(e) => (paged, false, Some(e.clone())),
        Entries::Spilled { count, spill_ref } => {
            let e: Vec<Entry> = pages(&files[spill_ref])
                .unwrap()
                .into_iter()
                .flat_map(|p| decode_spill_page(p, first, last).unwrap())
                .collect();
            assert_eq!(e.len() as u64, *count);
            (paged, true, Some(e))
        }
    }
}

/// Bytes of page `page` of a file that hold content (the rest is padding): the decoded
/// entries' or records' encoding plus the page header.
fn spill_page_content(page: &[u8], first: u64, last: u64) -> usize {
    let e = decode_spill_page(page, first, last).unwrap();
    6 + entries_len(&e, e[0].block)
}

fn bucket_page_content(page: &[u8], first: u64, last: u64) -> usize {
    let r = decode_bucket_page(page, first, last).unwrap();
    2 + r
        .iter()
        .map(|r| {
            let mut b = Vec::new();
            r.encode(&mut b, first);
            b.len()
        })
        .sum::<usize>()
}

/// Builds every case, checks that what it wrote reads back as its input and that it is the
/// edge it is meant to be, and writes the vectors.
fn render() -> String {
    let mut out = String::from(
        "# addr1 reader conformance vectors (eth2swarm src/addr/vectors.rs; format: \
         docs/address-index.md).\n\
         # Regenerate: ETH2SWARM_WRITE_VECTORS=1 cargo test --locked addr::vectors\n\
         # Every case is one slice. The payloads are those of the SOCs (not signed here); a\n\
         # file is listed with its Swarm reference. Everything listed was read back from these\n\
         # bytes by eth2swarm's decoders: the descriptor, the buckets' counts, the files'\n\
         # references, every address's entries and forms.\n\
         #   case <name> <what it covers>\n\
         #   slice <level> <index> <first unit of the index> <SliceRecord payload>\n\
         #   bucket <k> <BucketRecord payload>  (every bucket of the slice)\n\
         #   file <reference> <data>  (every spill and pages file of the slice)\n\
         #   address <address> <record: inline|spilled> <bucket: inline|paged> <count> <block.slot.roles,…>\n\
         #   absent <address>  (an address the slice has no record of)\n",
    );
    for case in cases() {
        let (level, index) = case.slice;
        let base = held_first_block(level, index, case.first_unit);
        assert!(
            case.addresses.iter().all(|(a, _)| !ignored(a)),
            "{}: an address detection never indexes",
            case.name
        );
        let apps: Vec<Appearance> = case
            .addresses
            .iter()
            .flat_map(|(a, es)| {
                es.iter().map(|e| Appearance {
                    block: e.block,
                    slot: e.slot,
                    address: *a,
                    roles: e.roles,
                })
            })
            .collect();
        let built = build_slice(
            case.slice,
            case.first_unit,
            &apps,
            B256::repeat_byte(index as u8 + level),
            role::mask(false),
        )
        .unwrap();
        // Everything below reads the descriptor from its bytes, as a reader does.
        let encoded = built.record.encode();
        let rec = SliceRecord::decode(&encoded).unwrap();
        assert_eq!(rec, built.record);
        assert_eq!(
            (rec.level, rec.index, rec.first_block),
            (level, index, base)
        );
        assert_eq!(rec.addresses as usize, case.addresses.len());
        assert_eq!(rec.appearances as usize, apps.len());
        let files: HashMap<B256, Vec<u8>> = built.files.iter().cloned().collect();
        for (r, f) in &built.files {
            assert_eq!(file_reference(f), *r, "{}: a file's reference", case.name);
        }
        // An address of no case: the slice must give nothing for it.
        let absent = Address::left_padding_from(&(0xdead_0000u64 + index as u64).to_be_bytes());
        let bucket = |a: &Address| bucket_of(&keccak256(a.as_slice()), rec.bucket_count);
        let mut buckets: Vec<u32> = case
            .addresses
            .iter()
            .map(|(a, _)| bucket(a))
            .chain([bucket(&absent)])
            .collect();
        buckets.sort();
        buckets.dedup();

        writeln!(out, "case {} {}", case.name, case.about).unwrap();
        writeln!(
            out,
            "slice {level} {index} {} {}",
            case.first_unit,
            hex::encode(&encoded)
        )
        .unwrap();
        // Every bucket, so a reader can recompute the descriptor's buckets root too.
        for (k, b) in built.buckets.iter().enumerate() {
            writeln!(out, "bucket {k} {}", hex::encode(b)).unwrap();
        }
        for (r, f) in &built.files {
            writeln!(out, "file {} {}", hex::encode(r), hex::encode(f)).unwrap();
        }
        let mut forms = Vec::new();
        for (a, es) in &case.addresses {
            let (paged, spilled, got) = read_back(&rec, &built.buckets, &files, a);
            assert_eq!(got.as_ref(), Some(es), "{}: {a} reads back", case.name);
            writeln!(
                out,
                "address {} {} {} {} {}",
                hex::encode(a),
                if spilled { "spilled" } else { "inline" },
                if paged { "paged" } else { "inline" },
                es.len(),
                line_entries(es)
            )
            .unwrap();
            forms.push((paged, spilled));
        }
        assert_eq!(read_back(&rec, &built.buckets, &files, &absent).2, None);
        writeln!(out, "absent {}", hex::encode(absent)).unwrap();

        // Each case is the edge it is named for.
        let (first, last) = (rec.first_block, rec.last_block());
        let inline_len: usize = BucketRecord::INLINE_HEADER
            + case
                .addresses
                .iter()
                .map(|(_, e)| record_len(e, base))
                .sum::<usize>();
        let file_pages = |i: usize| pages(&built.files[i].1).unwrap();
        match case.edge {
            Edge::SpillPages(n) => {
                assert_eq!(forms, [(false, true)]);
                assert_eq!((built.files.len(), file_pages(0).len()), (1, n));
            }
            Edge::SpillPageFull => {
                assert_eq!(forms, [(false, true)]);
                let p = file_pages(0);
                assert_eq!(p.len(), 2);
                assert_eq!(spill_page_content(p[0], first, last), PAGE_SIZE);
            }
            Edge::SpillLimit => {
                let l: Vec<usize> = case
                    .addresses
                    .iter()
                    .map(|(_, e)| entries_len(e, base))
                    .collect();
                assert_eq!(l, [SPILL_INLINE_MAX, SPILL_INLINE_MAX + 1]);
                assert_eq!(forms, [(false, false), (false, true)]);
            }
            Edge::Bucket(len, n) => {
                let one_bucket = case.addresses.iter().all(|(a, _)| bucket(a) == buckets[0]);
                assert!(one_bucket, "{}: one bucket", case.name);
                assert_eq!(inline_len, len, "{}", case.name);
                let paged = len > PAGE_SIZE;
                assert!(forms.iter().all(|f| f.0 == paged));
                if paged {
                    let pages_file = &built.files.last().unwrap().1;
                    assert_eq!(pages(pages_file).unwrap().len(), n);
                } else {
                    assert_eq!(built.buckets[buckets[0] as usize].len(), len);
                }
            }
            Edge::BucketPageFull => {
                assert!(forms.iter().all(|f| *f == (true, false)));
                let p = file_pages(0);
                assert_eq!(p.len(), 2);
                assert_eq!(bucket_page_content(p[0], first, last), PAGE_SIZE);
            }
            Edge::LateStart => {
                assert!(base > first_block(level, index));
                assert_eq!(forms, [(false, false), (false, true)]);
            }
            Edge::MainnetHeight => {
                assert!(base > 1 << 24);
                let e = &case.addresses[0].1;
                let deltas: Vec<usize> = std::iter::once(e[0].block - base)
                    .chain(e.windows(2).map(|w| w[1].block - w[0].block))
                    .map(varint_len)
                    .collect();
                assert_eq!(deltas, [1, 1, 3, 4]);
                assert_eq!(forms, [(false, false), (false, true)]);
            }
        }
        // A spilled record in a paged bucket.
        if case.name == "bucket-pages" {
            assert!(forms.contains(&(true, true)));
        }
    }
    out
}

#[test]
fn reader_vectors_are_what_the_builder_writes() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(PATH);
    let got = render();
    if std::env::var("ETH2SWARM_WRITE_VECTORS").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        got == want,
        "{PATH} differs from what the builder writes; if the format changed on purpose, \
         regenerate it with ETH2SWARM_WRITE_VECTORS=1"
    );
}
