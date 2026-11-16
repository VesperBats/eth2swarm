//! Swarm Binary Merkle Tree (BMT) hashing and content-addressed chunk (CAC) addresses.
//!
//! A chunk payload (≤ 4096 bytes) is zero-padded to 4096 bytes, split into 128 segments of
//! 32 bytes and hashed pairwise with keccak256 up to a single root. The chunk address is
//! `keccak256(span_le_u64 ‖ bmt_root)`, where `span` is the number of bytes the chunk covers
//! (for a single-chunk payload: its length).

use alloy::primitives::{B256, keccak256};

/// Maximum payload of a single Swarm chunk.
pub const CHUNK_SIZE: usize = 4096;
const SEGMENT_SIZE: usize = 32;

/// BMT root of a payload of at most [`CHUNK_SIZE`] bytes.
pub fn bmt_root(payload: &[u8]) -> B256 {
    assert!(
        payload.len() <= CHUNK_SIZE,
        "chunk payload exceeds {CHUNK_SIZE} bytes"
    );
    let mut level = vec![0u8; CHUNK_SIZE];
    level[..payload.len()].copy_from_slice(payload);
    while level.len() > SEGMENT_SIZE {
        level = level
            .as_chunks::<{ 2 * SEGMENT_SIZE }>()
            .0
            .iter()
            .flat_map(|pair| keccak256(pair).0)
            .collect();
    }
    B256::from_slice(&level)
}

/// Address of a content-addressed chunk with the given span and payload.
pub fn cac_address(span: u64, payload: &[u8]) -> B256 {
    let mut buf = [0u8; 8 + 32];
    buf[..8].copy_from_slice(&span.to_le_bytes());
    buf[8..].copy_from_slice(bmt_root(payload).as_slice());
    keccak256(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    #[test]
    fn matches_bee_single_chunk_reference() {
        // Reference returned by Bee 2.8.3 for `POST /bytes` with body "hello swarm".
        let payload = b"hello swarm";
        assert_eq!(
            cac_address(payload.len() as u64, payload),
            b256!("f9335c1f06d729ae3e725352a445fa01a1e9dc20e937f0ef19ab199b1fb2827b")
        );
    }
}
