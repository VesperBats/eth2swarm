//! The root reference of a Swarm file (`POST /bytes`), computed locally.
//!
//! Bee's chunker, unencrypted and without erasure coding (`Swarm-Redundancy-Level: 0`; Bee
//! 2.8.3 erasure-codes `/bytes` uploads unless told otherwise): the data is cut into
//! 4096-byte leaf chunks; up to 128 references (32 bytes each) form an intermediate chunk
//! whose span is the number of data bytes below it; levels are built until one reference is
//! left. When a level ends with a single reference that was not wrapped, that reference is
//! carried up unchanged instead of being wrapped on its own (Bee's hashtrie "carrier chunk").
//!
//! The address index needs this to put the reference of a spill or pages file into a bucket
//! before anything is uploaded, and independent importers need it to agree byte for byte.

use alloy::primitives::B256;

use super::bmt::{CHUNK_SIZE, cac_address};

/// References per intermediate chunk.
const BRANCHES: usize = CHUNK_SIZE / 32;

/// Root reference of `data` as Bee computes it for `POST /bytes`.
pub fn file_reference(data: &[u8]) -> B256 {
    if data.len() <= CHUNK_SIZE {
        return cac_address(data.len() as u64, data);
    }
    // levels[0] holds leaf references; each entry is (reference, span).
    let mut levels: Vec<Vec<(B256, u64)>> = vec![Vec::new()];
    for leaf in data.chunks(CHUNK_SIZE) {
        push(
            &mut levels,
            0,
            (cac_address(leaf.len() as u64, leaf), leaf.len() as u64),
        );
    }
    // Finish from the bottom: an empty level is skipped, a single reference is carried up,
    // anything else is wrapped into one chunk on the level above.
    let mut l = 0;
    loop {
        let top = levels.iter().skip(l + 1).all(Vec::is_empty);
        match levels[l].len() {
            0 => {}
            1 if top => return levels[l][0].0,
            1 => {
                let carried = levels[l].pop().unwrap();
                levels[l + 1].push(carried);
            }
            _ => {
                let wrapped = wrap(std::mem::take(&mut levels[l]));
                if levels.len() == l + 1 {
                    levels.push(Vec::new());
                }
                levels[l + 1].push(wrapped);
            }
        }
        l += 1;
        if levels.len() == l {
            levels.push(Vec::new());
        }
    }
}

/// Number of chunks Bee stores for a file of `len` bytes (leaves and intermediate chunks;
/// a carried single reference adds no chunk).
pub fn file_chunks(len: usize) -> usize {
    if len <= CHUNK_SIZE {
        return 1;
    }
    let mut refs = len.div_ceil(CHUNK_SIZE);
    let mut chunks = refs;
    while refs > 1 {
        let (full, rest) = (refs / BRANCHES, refs % BRANCHES);
        // Full groups are wrapped; a remainder of several references is wrapped too, a single
        // one is carried up as it is.
        chunks += full + usize::from(rest > 1);
        refs = full + usize::from(rest > 0);
    }
    chunks
}

/// Append a reference to level `l`, wrapping the level as soon as it is full.
fn push(levels: &mut Vec<Vec<(B256, u64)>>, l: usize, r: (B256, u64)) {
    if levels.len() == l {
        levels.push(Vec::new());
    }
    levels[l].push(r);
    if levels[l].len() == BRANCHES {
        let wrapped = wrap(std::mem::take(&mut levels[l]));
        push(levels, l + 1, wrapped);
    }
}

/// One intermediate chunk over `refs`.
fn wrap(refs: Vec<(B256, u64)>) -> (B256, u64) {
    let span: u64 = refs.iter().map(|(_, s)| s).sum();
    let payload: Vec<u8> = refs.iter().flat_map(|(r, _)| r.0).collect();
    (cac_address(span, &payload), span)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic test data.
    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn single_chunk_is_its_cac_address() {
        let d = data(4096);
        assert_eq!(file_reference(&d), cac_address(4096, &d));
    }

    #[test]
    fn chunk_counts() {
        for (len, chunks) in [
            (0, 1),
            (4096, 1),
            (4097, 3),
            // 128 leaves + their intermediate chunk.
            (128 * 4096, 129),
            // 129 leaves: one full intermediate chunk, the last leaf carried, and a root.
            (128 * 4096 + 1, 131),
            // 16384 leaves: 128 intermediates and a root.
            (16384 * 4096, 16513),
            // One more leaf, carried two levels up: 128 + 1 + 1 extra chunks.
            (16384 * 4096 + 1, 16515),
        ] {
            assert_eq!(file_chunks(len), chunks, "length {len}");
        }
    }

    #[test]
    fn two_leaves() {
        let d = data(4097);
        let a = cac_address(4096, &d[..4096]);
        let b = cac_address(1, &d[4096..]);
        let payload = [a.0, b.0].concat();
        assert_eq!(file_reference(&d), cac_address(4097, &payload));
    }

    /// References Bee 2.8.3 returned for `POST /bytes` with `Swarm-Redundancy-Level: 0` and
    /// [`data`] of these lengths (bee-factory, worker 1, 2026-11-18). Without that header
    /// this Bee erasure-codes the file, which changes the reference. They cover the
    /// level boundaries and the carried single reference.
    #[test]
    fn matches_bee_at_level_boundaries() {
        check(BEE_REFERENCES);
    }

    /// The two-levels-up cases hash 64 MiB, minutes in a debug build:
    /// `cargo test --release -- --ignored`.
    #[test]
    #[ignore]
    fn matches_bee_two_levels_up() {
        check(BEE_REFERENCES_LARGE);
    }

    fn check(vectors: &[(usize, &str)]) {
        for (len, expected) in vectors {
            assert_eq!(
                file_reference(&data(*len)).to_string(),
                *expected,
                "length {len}"
            );
        }
    }

    const BEE_REFERENCES: &[(usize, &str)] = &[
        (
            1,
            "0xfe60ba40b87599ddfb9e8947c1c872a4a1a5b56f7d1b80f0a646005b38db52a5",
        ),
        (
            4096,
            "0x6400c281c45741b07de93a8d45c7f1505a6c2891b5eb89064c39718e62cd4d04",
        ),
        (
            4097,
            "0x09354b8aa9bf76e7a3f9591397b07c98ddf373e24d6aae7d69de01803df644be",
        ),
        (
            8192,
            "0x3073a2e8f0607eb980fdcd6593c0948e916cac1d76c6e1dd8028d73f79b85a10",
        ),
        // 128 leaves: one full intermediate chunk.
        (
            524287,
            "0xd064c5989933c26efb72e38884bbfd4b475ac369d7aafaeab2cf3098e6d5bbff",
        ),
        (
            524288,
            "0x7e28d401302a8ffb4091e8ae4b2ca78b5dc74125b416cfbed6370d11f692beca",
        ),
        // 129 leaves: the last leaf is carried up next to the full intermediate chunk.
        (
            524289,
            "0x63f72f51ebe9996af2d9c9ded7062484af525f1676603b838d91abd20b8a132d",
        ),
        (
            528384,
            "0xf37c8588cb133b31480a8f217dd3b813830ccd7b23d199cc156f5b666389f2dd",
        ),
        (
            528385,
            "0xdd2227d2dfe2d89be7918b97758068ecbd2d9a32a2e03bbefc4c98f9285b2bc1",
        ),
        (
            1052673,
            "0xa9d7c1cd9079e8e6c557e51bff6a33cb4bb830f7cd54d5d845fabf55f33fa179",
        ),
    ];

    const BEE_REFERENCES_LARGE: &[(usize, &str)] = &[
        // 128 × 128 leaves: a full second level; one more byte carries a leaf two levels up.
        (
            67108864,
            "0xdb1c8871f904d8ed0211f6b626db34ab9bd3b40a4bb32c8e290e0e86f95a3943",
        ),
        (
            67108865,
            "0x56798f8f37109429eaf993e018c134a5429bd698fda937fa84b2eddf75e2d151",
        ),
        (
            67112961,
            "0x9259a99399b1d22581ed420a5f0f92b3d7e7a5e1f8b3e982d04e8248e4c6efde",
        ),
    ];
}
