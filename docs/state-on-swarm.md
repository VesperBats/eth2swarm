# State on Swarm (design sketch)

Status: **design sketch, not implemented.** Figures are **estimates** unless marked
*measured*. The measurements that would replace them are listed under *Open questions*.

Ethereum's current state (accounts, storage slots, contract code) stored on Swarm, so that a
light client can read it without a full-node peer and check every value against a state
root it already holds. The intended reader is [Myotis](https://github.com/biafra23/myotis),
which reads state over devp2p snap today.

This builds on Myotis's research note
[state-on-swarm.md](https://github.com/biafra23/myotis/blob/a88a36f440845d928adc9a880d471d9c7f892206/docs/ideas/state-on-swarm.md)
(2026-11-05, "the note" below). The note measured what a block rewrites and proposed
*pages*: one Swarm chunk per interior trie node, found by the node's hash, which also
carries the node's leaf children. A *jump table* per published root returns the top four
levels in one fetch. This document proposes a different layout for the same job and
compares the two.

The design in one paragraph: the state is stored **flat**. Only the leaves (accounts and
slots) are kept, in **buckets** that each hold the leaves below a fixed key prefix, so a
bucket is a whole subtree of the trie. A reader rebuilds the subtree's hash from the
leaves and follows a short chain of sibling hashes up to the state root. Changes are
published as one **slice** per block, built from the block's access list (BAL, EIP-7928)
and merged pairwise like a binary counter, which gives the slices the shape of a Merkle
Mountain Range. A reader takes one base bucket plus a few buckets per live slice, merges
them, and checks the result against the state root from the beacon light client. A
complete state snapshot is about 15 times smaller than in the note's layout, and a read
takes two or three round trips instead of 5–8. The price is several times more bytes per
cold read.

## Requirements

- **No publisher trust.** Nothing a reader accepts may rest on the publisher's word, its
  signature or its identity. Every value is checked against a commitment the consensus
  layer made. A publisher may only be able to withhold or delay.
- **What the reader needs.** Myotis reads three things, each pinned to one state root: an
  account, a storage slot, and code by its hash. It reads at the `finalized` root and at
  the head. A `latest` read must never be answered from an older root.
- **Swarm's rules**, as in the [address index](address-index.md):
  - A chunk holds at most 4,096 bytes and is paid for whole, full or not.
  - There are no queries. A reader can only ask for an address it computed itself.
  - Write once. An overwritten single-owner chunk (SOC) does not converge on one version.
  - Do not probe. A miss ends in a timeout, so every address a reader computes must exist.

## Trust

A reader accepts a value only after recomputing a keccak256 chain that ends in a hash it
already holds from the beacon light client. Everything fetched from Swarm is a hint.

| Fetched from Swarm | Accepted because |
|---|---|
| Account, or proof that it does not exist | the rebuilt trie root equals the block's state root |
| Storage slot, or proof that it is empty | the rebuilt storage root equals the `storageRoot` of the proven account |
| Contract code | `keccak256(code)` equals the `codeHash` of the proven account |
| Block access list (to reach the head) | `keccak256(BAL)` equals `block_access_list_hash` in a verified header |
| Base buckets, slices, sibling hashes, head record | not accepted on their own: they are inputs to the first two rows |

What follows from this:

- **A wrong hint cannot produce a wrong answer.** A forged leaf, a stale sibling hash, a
  slice left out of the head record or a faulty merge all change the recomputed root. The
  read then fails and falls back to another publisher or to snap.
- **Completeness is proven, not claimed.** The hash of a subtree commits to exactly the
  leaves below it. A reader that reproduces the hash knows it has all of them, so "this
  account does not exist" and "this slot is zero" are proven like any other value. (The
  address index cannot do this; a state trie can.)
- **The publisher signs nothing that matters.** The SOC signature and the postage stamp
  are Swarm's write and payment controls. The publisher's key decides where chunks are
  found, not whether they are right.
- **No publisher-made anchor.** A root over the slices (the Merkle Mountain Range's own
  root) would be the publisher's word, so no reader relies on one. See *Where the Merkle
  Mountain Range fits*.
- **What a publisher can still do:** withhold data, publish late, or publish garbage,
  which is detected. That is a liveness risk only.

The state roots come from headers the beacon light client vouches for: the finalized
block and the optimistic head, as Myotis's `ExecAnchor` holds them today.

## Layout

### Cutting the trie

A node's *depth* is the number of nibbles on its path, so the root is at depth 0. The
account trie is cut at fixed depths, which needs a branch node at every cut depth. On
mainnet every node down to depth 5 is a full branch node with 16 children: *measured* on
499 random paths, below. With about 26 accounts below every depth-6 node, a depth-5 node
that is not a branch is practically impossible. The cut for mainnet today:

| Tier | What it holds | How it changes |
|---|---|---|
| Crown | the hashes of all 256 nodes at depth 2 (8,192 bytes); the root follows from them | about 85 % of the hashes per block (the note counts about 220 of 256) |
| Upper | the 16 child hashes of a node at depth 2, and of a node at depth 3 | about 85 % of the depth-2 groups and 11 % of the depth-3 groups per block |
| Middle | the 16 child hashes of a node at depth 4 | a few hundred groups per block |
| Lower | the 16 child hashes of a node at depth 5 | a few hundred groups per block |
| Leaf bucket | the accounts below a 23-bit prefix: two neighbouring depth-6 subtrees, about 53 accounts | when one of its accounts changes |

A reader needs one group of 16 hashes per level on its key's path, four groups in all,
plus the crown and one leaf bucket.

**The depths are parameters of a chain and a generation**, and the head record names
them. Sepolia's account trie has full branch nodes down to depth 5 as well, but only
about 10 accounts below a depth-6 node (*measured*, below), so its buckets take a 22-bit
prefix. On mainnet a bucket holds about 53 accounts and a chunk takes about 87, so about
2 % of the buckets would be split once the account count has grown by a third. A later
generation then cuts at 24 bits: every base is written anew anyway.

**A bucket that does not fit in a chunk is split, not paged.** Its chunk then holds the
hash group of the next level in place of the leaves, and the leaves go to sub-buckets
with a longer prefix, which are split again if they must be. The reader learns this
from the chunk it fetched, so it never asks for one that may not exist, and pays one more
fetch per extra level.

Paging, as in the address index, would keep such a bucket writable but would not bound
what a reader pays. A reader has to hold every leaf of the subtree it rebuilds, and anyone
can force an overflow: an address whose hash falls below a chosen 23-bit prefix costs
about 2^23 hashes and a transfer of 1 wei (21,000 gas). In a paged bucket, 10,000 such
accounts would add about 470 KB to every read below that prefix, and nobody could remove
them. In a split bucket they add two or three levels, because every further level costs
the attacker 16 times more hashes. The trie itself answers the same attack the same way,
with a longer path.

The same rule covers companion and storage buckets, where slot keys can be ground through
token transfers to chosen addresses. A bucket that overflows within a generation is split
in the slices: the slice carries the hash group of the new level, and a reader then takes
only the leaf entries below its own sub-prefix. The buckets of a slice cover a key range
and can overflow too. Those are paged, sorted by key, and the reader finds its narrow
range by binary search.

Accounts are stored in snap's slim form (an empty `storageRoot` or `codeHash` is left out)
with the key bytes that the prefix does not already give. Interior trie nodes are stored
nowhere. The reader rebuilds them: inside a bucket from the leaves, above it from the
hash groups.

**Storage tries** are cut the same way, each with its own bucket depth, because their
sizes differ by eight orders of magnitude. A small trie goes whole into a *companion
bucket*, which it shares with the other small tries below the same account-key prefix. A
large one gets leaf buckets at the depth that fills a chunk, and hash groups for every
level above. It has no crown: its root is the `storageRoot` in the account, which the
reader has already proven. The bucket depth of a contract is recorded beside its account
in the base.

**Code** is stored once per code hash, as in the note: the code itself if it fits in a
chunk, else a reference to a Swarm file.

### Base

A *generation* `g` starts with a base: the complete flat state at one finalized block
`S`, a multiple of 2^20. Every address is computed from the key alone:

| Object | Found by | Content |
|---|---|---|
| Leaf bucket | generation, 23-bit key prefix | the accounts below the prefix, at block `S` |
| Path chunk | generation, 5-nibble key prefix | the four hash groups on that path, at block `S` (2,048 bytes) |
| Crown | generation | the 256 depth-2 hashes at block `S` (8,192 bytes; three chunks with their record headers) |
| Companion bucket | generation, 23-bit key prefix | the whole storage of every small contract below the prefix |
| Storage bucket, storage path chunk | generation, account hash, slot-key prefix | the same as the first two rows, for one large storage trie |
| Code | code hash | the code |

A path chunk repeats the upper groups of its neighbours. That costs about a million
half-full chunks, 2 % of the base, and saves the reader three fetches.

Nothing in a base is ever rewritten, and its addresses carry no block hash. A base is
therefore built only after block `S` is final. The next generation's base is produced by
the top merge of the slices (below), not by a second state dump.

### Slices

A slice holds what changed over a range of blocks, as of the range's last block:

- **Leaf entries:** key and new value of every account and slot that changed. A slot set
  to zero and an account removed from the trie are entries with an empty value.
- **Hash entries:** for every hash group that changed, its changed members: key prefix and
  new child reference.
- **The crown** at the range's last block.

Entries are bucketed by key range, in sections that follow the tiers above, so that a
reader fetches at most one bucket per tier: one or two buckets from a slice of a few
blocks, up to four from a slice of months. A group is never split across buckets.

A reader that merges the base with the slices up to block `N`, newest first and latest
value wins, holds the leaf bucket and the four hash groups exactly as they are at `N`.

Hash entries are what make this cheap. A changed branch node is about 360 bytes on
average, but it differs from its predecessor in one child. The entry that says so is
about 36 bytes.

### The mountain range

The slice of one block is a leaf. Two neighbouring slices of equal length are merged into
one of twice the length, latest value wins, exactly like the slices of the address
index. Slices are aligned to absolute block numbers and belong to no generation. The live
slices are the peaks of a Merkle Mountain Range over the blocks since `S`, and the
*cover* of `(S, N]` has one slice for every set bit of `N − S`: at most 20 within a
generation of 2^20 blocks (146 days), 10 on average.

Large merges run in the background while readers keep using the slices they replace (the
`pending` mechanism of the address index).

**Handover.** When the last block of a generation is final, the top slice, of all 2^20
blocks, is merged with the base into the base of the next generation, at `S + 2^20`.
Uploading it takes days: 3 at 200 chunks a second, 19 at 30. Until it is complete,
head records keep naming the old base, and the cover is the top slice plus the slices
after it: up to 21, and more buckets per read than the estimates below assume. The
first head record after that names the new base and drops the top slice. The slices
after it stay as they are.

A slice's address contains the hash of its last block. A slice built on a block that is
later reorganized away is therefore simply never asked for again. Slices can be
published before their blocks are final.

### Head record

One record per published block, found by the **block hash**: it lists the generation,
its cut depths and the cover (level, index, last block hash and bucket counts of each
live slice, and the pending merges). A reader that holds a verified header fetches the
record for its hash and can then compute every other address.

The record is a hint like the rest. A record that names the wrong slices leads to a read
that fails.

## Reading

An account at block `N`, whose state root `R` the reader holds:

1. Fetch the head record for `N`'s block hash.
2. In one parallel wave, fetch the base path chunk and leaf bucket, the crown of the
   newest slice (the base's, if the cover is empty), and from every slice in the cover
   the buckets its key falls in.
3. Merge, newest first: the leaf entries into the leaf bucket, the hash entries into the
   four groups.
4. Rebuild the bucket's subtrees from the leaves, put their hashes into the lowest group,
   hash that group's branch node, and so on up through the crown to the root.
5. If the result equals `R`, the account, or its absence, is proven. If not, the read has
   failed and nothing is returned.

That is two round trips: the head record, then the wave.

A storage slot repeats steps 2–5 inside the storage trie and compares with the account's
`storageRoot`. That is a third round trip, unless the reader has cached the contract's
bucket depth. In a slice, a contract's slot entries follow its account entry, so for a
small trie they are usually in a bucket the reader already has. Code is one fetch, or a
file.

Buckets of old slices never change, so a reader keeps them. A repeat read of the same key
one block later fetches only the newest slice's buckets.

### What a read reveals

Whoever serves a chunk learns its address, and the addresses are computed from the key:

- A base leaf bucket hides an account among the others below its prefix: about 53 on
  mainnet, fewer in a split bucket.
- A slice bucket covers a wider key range, the wider the smaller the slice.
- The address of a storage bucket of a large trie contains the account hash, so it names
  the contract, and its prefix narrows the slot.

A storer neighbourhood sees one of these. The Bee node or gateway a reader uses sees all
of them and can intersect them down to one base bucket. That is less than a snap request
shows a peer (the exact key), and more than a head read from BALs shows (nothing). A
reader that cares runs its own Bee node, as with the address index.

### Reads at the head

Publishing lags the chain by the time it takes to upload a slice, and a reader must not
probe for a head record that may not exist yet. So a head read takes two steps:

1. Read the key from Swarm at a block a few below the head, verified against that
   block's root.
2. Apply the block access lists from there to the head, each checked against its header.
   A BAL carries the new balance, nonce, code and slot values, so the value at the head
   follows without another proof.

Myotis's follower already does step 2, starting from a snap proof. Starting from Swarm it
needs a few BALs, if publishing keeps within a few blocks of the head. Starting from the
finalized root, as in the note, it needs 64–96 of them, 6–9 MiB.

Before Amsterdam there are no BALs, so nothing carries a Swarm read up to the head.
`latest` reads then stay on snap. Swarm serves `finalized` reads and reads pinned to a
block that is already published.

## Publishing

From Amsterdam on, the publisher needs neither an execution client's database nor an EVM:

1. Follow headers and BALs over devp2p (eth/71 or snap/2).
2. Apply each BAL's new values to its own copy of the leaf buckets, and remove the
   accounts the protocol removes (a BAL carries values, not removals).
3. Recompute the hashes of the touched buckets and of every group above them.
4. Compare the resulting root with the header's state root. Equality proves that the
   slice is right. This step also yields the new storage roots, which a BAL does not
   carry.
5. Upload the block's slice, then the head record, then any merge that is due.

The publisher's working data is the layout itself: the flat buckets and the hash groups.
A second publisher, or an auditor, can rebuild any slice from one BAL and any merge
from its two halves. That is a way to find a broken publisher early. Readers do not
depend on it.

Before Amsterdam the changed keys come from a tracing node (`debug_traceBlockByNumber`,
prestate tracer in diff mode), as in the note's churn script.

The first base needs the complete flat state at one block, from a node's snapshot. Every
later base comes out of the top merge.

## Where the Merkle Mountain Range fits

- **It fits the time axis.** It gives the slices their shape: which ones exist, how any
  range since the base is covered by a logarithmic number of them, and how a reader that
  is `d` blocks behind catches up with `⌈log2(d + 1)⌉` slices. It also makes history a
  retention choice (see *Options*).
- **Its root is not a trust anchor.** An MMR root is a commitment by whoever built it. The
  consensus layer commits to three things here, and readers use only those:
  - the state root in every header;
  - the BAL hash in every header, from Amsterdam on;
  - the beacon chain's own accumulators (`historical_summaries`), for the header of an
    old block.
- **It cannot commit to the state itself.** An MMR over state writes cannot prove that a
  value is the latest one, which needs a proof that no later write exists. The trie root
  proves exactly that.

So the state root verifies, the BAL feeds the publisher and carries reads to the head,
and the mountain range arranges the slices.

## Numbers

### Measured

The account trie, sampled on 2026-11-17 at mainnet blocks 26,435,419 and following,
on the geth 1.17.7 full node the README names as the default `--rpc`:
`scripts/trie-shape.py` asks `eth_getProof` for random addresses, each of which returns
the nodes on the path of a random key (500 requests, one timed out).

| Depth | Paths with a branch node there | Occupied child slots, mean | Node size, mean |
|---|---|---|---|
| 0–5 | 499 of 499 | 16.00 of 16 (every node full) | 532 B |
| 6 | 499 of 499 | 12.92 | 433 B |
| 7 | 230 | 2.61 | 103 B |
| 8 | 6 | 2.00 | 83 B |

- A path has 7–10 nodes, 7.9 on average, 3.7 KB. (These are proofs of absence; the note
  measured 8.9 nodes for existing accounts.)
- 12.92 of 16 slots occupied at depth 6 means about 1.65 accounts per slot: about 26
  accounts below a depth-6 node and about 440 M in all, in line with the 430 M below.
- 209 paths ended at the leaf of a neighbouring account, which gives a sample of real
  accounts: 49 have code, 11 have storage. In slim form they take 4–68 bytes, 16.1 on
  average, against 107 bytes for the full leaf node.

The same script on Sepolia, on 2026-11-17 at blocks 12,155,236 and following, through a
public RPC endpoint, 300 paths:

| Depth | Paths with a branch node there | Occupied child slots, mean | Node size, mean |
|---|---|---|---|
| 0–5 | 300 of 300 | 16.00 of 16 (every node full) | 532 B |
| 6 | 300 of 300 | 7.20 | 250 B |
| 7 | 33 | 2.15 | 88 B |

- 7.20 of 16 slots occupied means about 0.6 accounts per slot: about 10 accounts below a
  depth-6 node and about 160 M in all.
- 98 paths ended at an account leaf: 60 have code, 12 have storage; 29.9 bytes in slim
  form on average.

Inputs taken from the note (measured there on 2026-11-05, or cited from ethPandaOps):

| | Mainnet |
|---|---|
| Accounts / storage slots | 430 M / 1.69 B |
| Account-trie / storage-trie nodes | 597 M (73 GB) / 2.29 B (247 GB) |
| Distinct contract codes | 3.2 M, 16.4 GB, about 7 M chunks |
| Changed per block | 494 accounts, 913 slots |
| New trie nodes per block | 7,397 (account trie 3,122, storage tries 4,275), 2.5 MiB |
| Distinct accounts changed per 32 blocks | about 8,300 |
| BAL size per block, compressed | about 92 KiB |
| Swarm's unique capacity | about 4.3 B chunks |

The head header of the same node (block 26,435,321) has no `block_access_list_hash`:
Amsterdam is not live on mainnet.

### Estimates

**Base.**

| Part | Derivation | Chunks |
|---|---|---|
| Account buckets | 2^23 buckets of about 53 accounts, about 47 bytes each (30 of key, 16 of account, 1 of length): 2.5 KB | 8.4 M |
| Account path chunks | 16^5 | 1.0 M |
| Storage and companion buckets | 1.69 B slots × about 48 bytes, at 2.8 KB per chunk; more if the small tries pack badly | about 29–35 M |
| Storage path chunks | one hash per bucket and level above it | about 2–3 M |
| Code | from the note | 7 M |
| **Total** | | **about 45–55 M** |

That is a little over 1 % of Swarm. The slot size and the split between small and large
storage tries are guesses; the account figures follow from the sample above.

**One block's slice.**

| Part | Derivation | Size |
|---|---|---|
| Account entries | 494 × about 60 bytes | 30 KB |
| Slot entries | 913 × about 50 bytes | 46 KB |
| Account-trie hash entries | about 1,940 changed nodes below the crown × 35 bytes | 68 KB |
| Storage-trie hash entries | very rough: about 1,500 × 40 bytes | 60 KB |
| Crown | | 8 KB |
| **Total** | | **about 210 KB, 85 chunks** |

The chunk count assumes the address index's bucket target of 2.8 KB (76 chunks) and a
partly filled last bucket in each section.

**Upload.** 85 chunks per block are 0.61 M chunks a day at the leaves. Above them the
publisher writes what the address index writes: only the slice a cover gains, so a block
is rewritten in about half of the levels, not in all of them. Merged slices also shrink
where writes repeat. How much is not measured for this layout. The note's nearest
figures are for 32 blocks: the distinct changed accounts are about 52 % of the per-block
changes summed, the new trie nodes 57 %. Assuming slices shrink to about half at 32
blocks, a fifth at one day and a twentieth at a generation, the merges add about 3.3
times the leaves. With every block's own slice uploaded first, the total is **about
2.6 M chunks a day**, 30 a second. The base adds 45–55 M chunks once per generation, in
a burst (see *Handover*). Only the cover is live, so old slices expire, in postage
batches grouped by lifetime as in the address index.

**Read.**

| Read | Chunks | Round trips |
|---|---|---|
| Account, nothing cached | about 30 on average, up to about 60 (roughly 60–130 KB) | 2: the head record, then one wave |
| Slot in a large storage trie, nothing cached | about as much again | 1 more |
| Slot in a small storage trie | the companion bucket, and few slice buckets beyond the account's | 1 more |
| Same key again, `d` blocks later | the buckets of up to `⌈log2(d + 1)⌉` new slices | 2 |

A slice costs 2 buckets up to roughly 2,000 blocks, 3 up to roughly 64,000 and 4 above
(the limits move with how much merged slices shrink). The average assumes that half of the
20 levels are present. The base adds a path chunk and a leaf bucket, the crown three
chunks. The byte figures assume about 2 KB per chunk: buckets with leaf entries near
2.8 KB, buckets with only hash groups about 1 KB. All of them assume buckets that are
not split.

### Compared with the note's layout

| | Pages by node hash (the note) | Leaf buckets and slices (this document) |
|---|---|---|
| Verified against | state root | state root |
| Base snapshot | 0.76 B chunks, 18 % of Swarm | about 45–55 M chunks, a little over 1 % |
| Upload | 24.7 M chunks a day (per epoch) | about 2.6 M chunks a day (per block) |
| Stored over time | grows with every published root that is kept | one base plus the live slices; the rest expires |
| Account read, cold | about 5 fetches, one after another | about 30 chunks in 2 round trips |
| ERC-20 balance, cold | about 8 fetches, one after another | about 60 chunks in 3 round trips |
| Keys ground into one subtree | the path grows by a node per 16-fold attacker effort | the same, with split buckets: one fetch per extra level |
| Historical state | every published root that is kept | none, unless old bases and inner slices are kept (see *Options*) |
| Publisher needs | a node with proofs or a full trie copy | headers and BALs, from Amsterdam on |

This layout spends reader bandwidth to save network capacity and round trips.
A cold `eth_call` that touches 50 keys fetches about 1,500–3,000 chunks. That is
10–20 s of the free allowance the note measured (about 140 chunks a second), against a
few hundred chunks for pages. Whether the trade is right depends on the per-chunk latency
and on how much a reader keeps cached, both unmeasured.

## Options

- **Fewer slices per read.** Merging four or eight slices at a time, or keeping one slice
  per level and rewriting it as it grows, shortens the cover and raises the upload. A
  shorter generation does the same. These are parameters, to be set from measurements.
- **Historical state.** As described, nothing older than the live cover can be read: an
  old base expires with the slices it belonged to. A read at an old block `X` needs the
  base of `X`'s generation and the cover of `(S, X]`, which is the set of peaks as they
  were at `X`. Keeping every base, and every slice above a chosen length instead of only
  the current peaks, keeps the state readable at that granularity: every epoch, every
  day. The cost is one base per generation (45–55 M chunks every 146 days, about 3 % of
  Swarm a year) plus the kept slices, at most 0.3 M chunks a day for each level kept. The
  old header must then be verified through the beacon chain's accumulators, which is the
  [block store](soc-block-store.md)'s job; proofs for it can be stored on Swarm as hints.
- **BAL archive.** Storing the BALs themselves costs about 23 chunks per block, 60 M a
  year, and keeps slices rebuildable after execution peers have dropped them. How long
  peers keep BALs is not checked here.

## Open questions

- **Storage tries with irregular shapes.** The top of the account trie is always full
  branch nodes, so fixed depths work. A storage trie can have an extension node or a
  single leaf across a tier boundary, and slots can be deleted. The layout needs a rule
  for such tries: per-trie depths that follow the actual shape, or hash entries that carry
  an extension's path. Until that rule exists the design covers the account trie only.
- **Removed accounts.** The BAL-driven publisher has to derive which accounts a block
  removed (an account left empty, a contract created and destroyed in one transaction)
  from the protocol's rules. A mistake shows as a root mismatch in its own check, so it
  stops the publisher but cannot mislead a reader.
- **Embedded nodes.** A child shorter than 32 bytes is embedded in its parent, not
  referenced by hash. It occurs in storage tries only. Hash entries must carry the child
  reference exactly as the parent holds it.
- **Finding the newest published block without probing.** The reader asks for the head
  record of a block a few below the head. How many depends on the upload lag. A record
  found by block number would be overwritten on a reorg, which the write-once rule
  forbids.
- **Reader cost.** A cold `eth_call` is far more expensive than with pages (above).
  Fewer slices per read, a warm cache and BAL following for watched accounts reduce it;
  by how much is open.
- **Split buckets.** The rule is stated, not specified: the record for a split in the
  base and in a slice, and what a reader pays below a prefix someone has ground keys
  into, are still to be written down.
- **Measure before building:**
  1. Entry sizes and bucket fill, from a full flat-state dump: accounts, slots, and the
     size distribution of storage tries.
  2. How much merged slices shrink at 2^5 … 2^20 blocks, and the share of the state that
     changes within one generation. Both set the upload and the size of the live slices.
  3. Upload throughput, and how soon a pushed chunk can be fetched from another node. This
     decides how close to the head Swarm can serve.
  4. Per-chunk latency and miss rate from a phone, as in the note.
- **Proof of concept on Sepolia**, as the note proposes: Amsterdam is live there, so the
  BAL-driven publisher and the head reads can be tried end to end.
