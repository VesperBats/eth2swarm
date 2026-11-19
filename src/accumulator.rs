//! Header accumulator: a two-level keccak256 Merkle commitment to the block-hash chain.
//!
//! Blocks are grouped into fixed-size *epochs* starting at block 0. Each epoch's block hashes
//! form a Merkle tree (the *epoch root*). The epoch roots form a second tree, whose root is
//! mixed with the epoch geometry into the *trust anchor*:
//!
//! ```text
//! anchor = keccak256(master_tree_root ‖ num_epochs (u64 BE) ‖ epoch_size (u64 BE))
//! ```
//!
//! A client that hard-codes the anchor can verify any header in the covered range with
//! `log2(epoch_size) + ceil(log2(num_epochs))` sibling hashes, without downloading other
//! headers. This mirrors the Portal Network's pre-merge accumulator (which uses SSZ/sha256
//! instead of keccak).

use alloy::primitives::{B256, keccak256};

/// Hash of two sibling nodes.
fn parent(left: &B256, right: &B256) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left.as_slice());
    buf[32..].copy_from_slice(right.as_slice());
    keccak256(buf)
}

/// Number of tree levels needed for `n` leaves (`ceil(log2(n))`, 0 for n ≤ 1).
pub fn depth_for(n: usize) -> u32 {
    n.max(1).next_power_of_two().trailing_zeros()
}

/// All levels of a Merkle tree over `leaves`, zero-padded to `2^depth` leaves.
/// `levels[0]` are the leaves, the last level holds the root.
fn levels(leaves: &[B256], depth: u32) -> Vec<Vec<B256>> {
    let width = 1usize << depth;
    assert!(leaves.len() <= width, "too many leaves for depth {depth}");
    let mut level = leaves.to_vec();
    level.resize(width, B256::ZERO);
    let mut out = vec![level];
    for _ in 0..depth {
        let next = out
            .last()
            .unwrap()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[l, r]| parent(l, r))
            .collect();
        out.push(next);
    }
    out
}

/// The root of [`Tree::new`]`(leaves, depth)` without keeping its levels: the leaves are folded
/// in place, and the zero-padding is folded in as the hashes of all-zero subtrees. Memory is
/// the leaves themselves (for a slice with millions of buckets).
pub fn root_of(mut leaves: Vec<B256>, depth: u32) -> B256 {
    assert!(
        leaves.len() <= 1usize << depth,
        "too many leaves for depth {depth}"
    );
    let mut zero = B256::ZERO; // the root of an all-zero subtree of the current height
    if leaves.is_empty() {
        leaves.push(B256::ZERO);
    }
    for _ in 0..depth {
        if leaves.len() % 2 == 1 {
            leaves.push(zero);
        }
        let n = leaves.len() / 2;
        for i in 0..n {
            leaves[i] = parent(&leaves[2 * i], &leaves[2 * i + 1]);
        }
        leaves.truncate(n);
        zero = parent(&zero, &zero);
    }
    leaves[0]
}

/// A Merkle tree with precomputed levels, for producing many proofs.
pub struct Tree {
    levels: Vec<Vec<B256>>,
}

impl Tree {
    pub fn new(leaves: &[B256], depth: u32) -> Self {
        Self {
            levels: levels(leaves, depth),
        }
    }

    pub fn root(&self) -> B256 {
        self.levels.last().unwrap()[0]
    }

    /// Sibling hashes from leaf `index` up to (excluding) the root.
    pub fn proof(&self, mut index: usize) -> Vec<B256> {
        let mut proof = Vec::with_capacity(self.levels.len() - 1);
        for level in &self.levels[..self.levels.len() - 1] {
            proof.push(level[index ^ 1]);
            index >>= 1;
        }
        proof
    }
}

/// Root implied by `leaf` at `index` and its sibling path.
pub fn root_from_proof(leaf: B256, mut index: usize, proof: &[B256]) -> B256 {
    let mut node = leaf;
    for sibling in proof {
        node = if index & 1 == 0 {
            parent(&node, sibling)
        } else {
            parent(sibling, &node)
        };
        index >>= 1;
    }
    node
}

/// The trust anchor for a master tree root and epoch geometry.
pub fn anchor(master_root: B256, num_epochs: u64, epoch_size: u64) -> B256 {
    let mut buf = [0u8; 32 + 8 + 8];
    buf[..32].copy_from_slice(master_root.as_slice());
    buf[32..40].copy_from_slice(&num_epochs.to_be_bytes());
    buf[40..].copy_from_slice(&epoch_size.to_be_bytes());
    keccak256(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proofs_verify_for_every_leaf() {
        let leaves: Vec<B256> = (0u64..5).map(|i| keccak256(i.to_be_bytes())).collect();
        let depth = depth_for(leaves.len());
        assert_eq!(depth, 3);
        let tree = Tree::new(&leaves, depth);
        for (i, leaf) in leaves.iter().enumerate() {
            let proof = tree.proof(i);
            assert_eq!(proof.len(), 3);
            assert_eq!(root_from_proof(*leaf, i, &proof), tree.root());
            assert_ne!(root_from_proof(*leaf, i ^ 1, &proof), tree.root());
        }
    }

    #[test]
    fn single_leaf_tree_is_the_leaf() {
        let leaf = keccak256(b"x");
        let tree = Tree::new(&[leaf], depth_for(1));
        assert_eq!(tree.root(), leaf);
        assert!(tree.proof(0).is_empty());
    }

    #[test]
    fn root_of_matches_the_tree() {
        for n in 0..70usize {
            let leaves: Vec<B256> = (0..n)
                .map(|i| keccak256((i as u64).to_be_bytes()))
                .collect();
            for depth in depth_for(n)..depth_for(n) + 3 {
                assert_eq!(
                    root_of(leaves.clone(), depth),
                    Tree::new(&leaves, depth).root(),
                    "{n} leaves, depth {depth}"
                );
            }
        }
    }
}
