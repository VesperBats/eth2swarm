//! Reader conformance vectors: slices built by the slice builder at the edges of the format
//! (a spill file of several pages, an address at the inline limit and one byte over, a bucket
//! of exactly one chunk and one byte over, which is paged, a pages file of several pages, a
//! slice of an index that starts inside it), written to `testdata/addr1-reader-vectors.txt`
//! for independent readers (Myotis' `myotis-addr1`) to decode and compare. Every address's
//! entries and forms are read back from the written bytes with this crate's decoders before
//! they are listed, and the test fails when the builder's output changes: a change of the
//! format then shows up as a diff of that file. Regenerate with
//! `ETH2SWARM_WRITE_VECTORS=1 cargo test --locked addr::vectors`.

use std::collections::HashMap;
use std::fmt::Write as _;

use alloy::hex;
use alloy::primitives::{Address, B256, keccak256};

use super::detect::{Appearance, role};
use super::layout::{
    AddressRecord, BucketBody, BucketRecord, Entries, Entry, PAGE_SIZE, SPILL_INLINE_MAX,
    SliceRecord, decode_bucket_page, decode_spill_page, entries_len, first_block, held_first_block,
    pages, varint_len,
};
use super::slice::{SliceId, bucket_of, build_slice, fingerprint};

const PATH: &str = "testdata/addr1-reader-vectors.txt";

/// What a case is the edge of, checked after it is built.
enum Edge {
    /// One spill file of this many pages.
    SpillPages(usize),
    /// The first address's inline entries are exactly the inline limit, the second's one byte
    /// more.
    SpillLimit,
    /// The addresses fill one bucket of exactly this many bytes inline (paged above
    /// [`PAGE_SIZE`]), its pages file of this many pages if paged.
    Bucket(usize, usize),
    /// An index that starts inside the slice: entries are relative to the held first block.
    LateStart,
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

/// Addresses whose hash starts below 0x55: bucket 0 of a slice of two or three buckets.
fn low_addresses() -> impl Iterator<Item = Address> {
    (1u64..)
        .map(|i| Address::left_padding_from(&i.to_be_bytes()))
        .filter(|a| keccak256(a.as_slice())[0] < 0x55)
}

/// Bytes of an inline record of `e`.
fn record_len(e: &[Entry], base: u64) -> usize {
    8 + varint_len((e.len() as u64) << 1) + entries_len(e, base)
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
    let a = |i: u64| Address::left_padding_from(&(0xa000 + i).to_be_bytes());
    let base = |index: u32| held_first_block(0, index, 0);
    vec![
        Case {
            name: "spill-pages",
            about: "one address with 2000 entries: a spill file of two pages",
            slice: (0, 1),
            first_unit: 0,
            addresses: vec![(
                a(1),
                (0..2000u32)
                    .map(|j| entry(base(1) + (j / 32) as u64, j % 32 + 1))
                    .collect(),
            )],
            edge: Edge::SpillPages(2),
        },
        Case {
            name: "spill-limit",
            about: "entries of exactly 256 bytes stay inline, 257 bytes spill",
            slice: (0, 2),
            first_unit: 0,
            addresses: vec![
                (a(2), entries(base(2), 84, 1)),
                (a(3), entries(base(2), 83, 2)),
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
            about: "a bucket of 64 addresses: paged, its pages file three pages",
            slice: (0, 5),
            first_unit: 0,
            addresses: full_bucket(base(5), 64, 40, 0),
            edge: Edge::Bucket(
                BucketRecord::INLINE_HEADER + 64 * record_len(&entries(0, 40, 0), 0),
                3,
            ),
        },
        Case {
            name: "late-start",
            about: "slice (2, 0) of an index from unit 3 (block 192), roles of several bits, \
                    one address spilled",
            slice: (2, 0),
            first_unit: 3,
            addresses: vec![
                (
                    a(4),
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
                    a(5),
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
    ]
}

fn line_entries(e: &[Entry]) -> String {
    e.iter()
        .map(|e| format!("{}.{}.{}", e.block, e.slot, e.roles))
        .collect::<Vec<_>>()
        .join(",")
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
    let b = BucketRecord::decode(&buckets[k as usize], first).unwrap();
    assert_eq!((b.level, b.index, b.bucket), (rec.level, rec.index, k));
    let (paged, records): (bool, Vec<AddressRecord>) = match b.body {
        BucketBody::Inline(r) => (false, r),
        BucketBody::Paged { pages_ref, .. } => (
            true,
            pages(&files[&pages_ref])
                .unwrap()
                .into_iter()
                .flat_map(|p| decode_bucket_page(p, first, last).unwrap())
                .collect(),
        ),
    };
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

/// Builds every case, checks that what it wrote reads back as its input and that it is the
/// edge it is meant to be, and writes the vectors.
fn render() -> String {
    let mut out = String::from(
        "# addr1 reader conformance vectors (eth2swarm src/addr/vectors.rs; format: \
         docs/address-index.md).\n\
         # Regenerate: ETH2SWARM_WRITE_VECTORS=1 cargo test --locked addr::vectors\n\
         # Every case is one slice. The payloads are those of the SOCs (not signed here); a\n\
         # file is listed with its Swarm reference. Every address's entries were read back from\n\
         # these bytes by eth2swarm's decoders.\n\
         #   case <name> <what it covers>\n\
         #   slice <level> <index> <first unit of the index> <SliceRecord payload>\n\
         #   bucket <k> <BucketRecord payload>  (the buckets the case's addresses fall into)\n\
         #   file <reference> <data>  (every spill and pages file of the slice)\n\
         #   address <address> <record: inline|spilled> <bucket: inline|paged> <count> <block.slot.roles,…>\n\
         #   absent <address>  (an address the slice has no record of; its bucket is listed)\n",
    );
    for case in cases() {
        let (level, index) = case.slice;
        let base = held_first_block(level, index, case.first_unit);
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
        let rec = &built.record;
        assert_eq!(rec.first_block, base);
        let files: HashMap<B256, Vec<u8>> = built.files.iter().cloned().collect();
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
            hex::encode(rec.encode())
        )
        .unwrap();
        for k in &buckets {
            writeln!(
                out,
                "bucket {k} {}",
                hex::encode(&built.buckets[*k as usize])
            )
            .unwrap();
        }
        for (r, f) in &built.files {
            writeln!(out, "file {} {}", hex::encode(r), hex::encode(f)).unwrap();
        }
        let mut forms = Vec::new();
        for (a, es) in &case.addresses {
            let (paged, spilled, got) = read_back(rec, &built.buckets, &files, a);
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
        assert_eq!(read_back(rec, &built.buckets, &files, &absent).2, None);
        writeln!(out, "absent {}", hex::encode(absent)).unwrap();

        // Each case is the edge it is named for.
        let inline_len: usize = BucketRecord::INLINE_HEADER
            + case
                .addresses
                .iter()
                .map(|(_, e)| record_len(e, base))
                .sum::<usize>();
        let file_pages = |i: usize| pages(&built.files[i].1).unwrap().len();
        match case.edge {
            Edge::SpillPages(n) => {
                assert_eq!(forms, [(false, true)]);
                assert_eq!((built.files.len(), file_pages(0)), (1, n));
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
                assert!(forms.iter().all(|f| *f == (paged, false)));
                if paged {
                    assert_eq!((built.files.len(), file_pages(0)), (1, n));
                } else {
                    assert_eq!(built.buckets[buckets[0] as usize].len(), len);
                }
            }
            Edge::LateStart => {
                assert!(base > first_block(level, index));
                assert_eq!(forms, [(false, false), (false, true)]);
            }
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
