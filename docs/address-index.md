# Address index (design)

Status: **implemented on a dev chain; mainnet scale in progress** (#8). Detection
(`src/addr/detect.rs`, `ethswarm appearances`), the record formats (`src/addr/layout.rs`),
slice building and the cover math (`src/addr/slice.rs`), measuring (`ethswarm addr-stats`),
the importer (`ethswarm addr-import`) with a full read-back check (`ethswarm addr-verify`),
and a client (`ethswarm addr-lookup`, head discovery from a finalized block, incremental
sync, and a check against the chain). An index can start at a later block instead of
genesis (`addr-import --from-block`, *Starting at a later block*). `ethswarm devnet-activity` generates test traffic on
a dev chain. The client reads every slice and whole spill and pages files; steps 3–5 of
*Client lookup* (page search, last page first, stopping when the screen is full) and the
time-based head estimate are not implemented. Still open:
building the large levels of a mainnet import from disk (the importer builds in memory up to
`--max-level`, 8 by default), postage batches grouped by lifetime (*Lifetime* below; one
`--batch` so far), and re-measuring with traces. Design issue: #5.

An index from an **address** to the transactions it **appears in**, stored on Swarm next to
the [SOC block store](soc-block-store.md). A mobile wallet uses it to list an address's
history, newest first, and to pick up new transactions incrementally. It needs no bulk
download: TrueBlocks' Unchained Index asks a client to scan about 6 GB of bloom filters;
here a full-history lookup reads 13 buckets of about 3 KB at today's height (measured below).

The design in one paragraph: the chain is cut into **units** of 64 finalized blocks. Each
unit gets an immutable index **slice**. Neighbouring slices of equal length are merged
into one slice of twice the length, like a binary counter, up to a cap of 2^16 units
(about 1.6 years). Any prefix of the chain is therefore covered by a handful of slices:
13 at mainnet's current height, at most 22 until late 2027. Inside a slice, addresses
are hashed into **buckets** of about 2.8 KB, and each bucket is one SOC at a
deterministic identifier. A lookup reads one bucket per slice. Nothing is ever
overwritten: only a head feed advances. Large merges run in the background while clients
keep using the smaller slices they replace.

## Design constraints

These follow from how Swarm and the block store work; the discussion is in #5.

- **Write once.** Bee can replace a SOC that has a newer stamp, but stale copies stay in
  neighbourhood replicas and forwarding caches, and a reader cannot tell which version it
  got. Index chunks are therefore never updated in place.
- **Do not probe for chunks that may not exist.** A retrieval miss in Swarm ends in a
  timeout, not a fast "not found". Every identifier a client computes must exist, or the
  client must have a way to know it does not.
- **No chunk per address.** Mainnet has hundreds of millions of distinct addresses (3.8
  million in the last 16,384 blocks alone), most with a handful of appearances. A stamped
  4 KB chunk each would cost terabytes of postage for about 70 GB of data.
- **Some privacy.** An identifier derived from a single address tells the storer
  neighbourhood which address someone looks up. A bucket hides the address among the other
  addresses in it: about 180 in 64-block slices, 127 in 16,384-block slices (measured),
  fewer in top-level slices. Whoever sees a client's whole lookup (the Bee node or gateway
  the wallet uses) can intersect the hash ranges of the buckets it asks for across slices,
  which differ because `B` differs, and is left with a fraction of one bucket. This is much
  weaker than scanning every bloom filter; a wallet that cares runs its own Bee node or
  fetches decoy buckets.
- **Finalized blocks only.** Slices are immutable, so they must not contain blocks that can
  still be reorganized. A unit of 64 blocks is about two beacon-chain epochs. Units are not
  aligned to epochs, and a block is finalized up to about three epochs after it, so the
  index runs about 13–32 minutes behind the tip, plus the time to build and upload a small
  slice (large merges do not hold the head back, see *Writing*).

## Appearances

An *appearance* is a pair (block, slot) at which an address occurs, with a set of roles.

`slot` is `0` for block-level appearances (fee recipient, withdrawal recipient, uncle
miner, reward author) and `txIndex + 1` for an appearance in a transaction.

`role` is a bit set:

| Bit | Name | Meaning |
|---|---|---|
| `0x01` | `FROM` | sender of the transaction |
| `0x02` | `TO` | recipient, or the contract the transaction created |
| `0x04` | `EMITTER` | address of a log the transaction emitted |
| `0x08` | `TOPIC` | address in a log topic (ERC-20/721/1155 `from`/`to`, …) |
| `0x10` | `WORD` | address-shaped 32-byte word in input data, init code, log data, or an internal call's input or output |
| `0x20` | `TRACE` | address in a call frame, creation or self-destruct (requires traces) |
| `0x40` | `BLOCK` | block-level: fee recipient, uncle miner, reward author, withdrawal recipient |
| `0x80` | `AUTH` | authority of an EIP-7702 authorization |

### Detection rules

Independent importers must produce identical slices (see *Verification*), so these rules
are normative. They are implemented in `src/addr/detect.rs` and checked by
`ethswarm appearances <block> [--traces]`.

The rules are a **superset of TrueBlocks'** (`trueblocks-chifra`, `pkg/uniq`, at
`ff9df4f`): the same sources (traces, receipts, withdrawals, miner) and the same word
heuristic, plus log emitters, known token events regardless of value, and EIP-7702
authorities. Every appearance TrueBlocks' scraper finds is found here too, which makes its
published index a test oracle for this one: its transaction index `i` is slot `i + 1`
here, and its block-level pseudo-indexes 99995–99999 (withdrawal, rewards) are slot 0. Two
exceptions:

- TrueBlocks records a reward to a precompile address under a sentinel address; here it is
  ignored like every other address up to `0x…ffff`.
- TrueBlocks indexes the **genesis allocations** as block 0, from a per-chain `allocs.csv`.
  Genesis state is not in any block, so this detection does not see it; see *Open
  questions*.

- *Address-shaped* (TrueBlocks' "implicit address"): a 32-byte word whose first 12 bytes
  are zero, whose value is at least 2^104 (one of bytes 12–18 is non-zero, so an address
  may have up to six leading zero bytes), and whose last four bytes are not all zero
  (that is a round amount far more often than an address).
- *Words of* some data at offset `o`: the 32-byte windows at byte offsets `o + 32·k` that
  fit completely; a shorter remainder is ignored.
- **Addresses up to `0x…ffff`** (the zero address and the precompiles) are **never
  indexed**, in any role.
- `FROM`: the transaction's sender. `TO`: its `to`; for a contract creation the address
  `keccak256(rlp([sender, nonce]))[12..32]`, from the transaction's sender and nonce, whether
  the creation succeeded or not (the consensus receipt carries no contract address; the
  `contractAddress` of an RPC receipt is derived by the node). Failed transactions keep
  `FROM`, `TO` and their input `WORD`s; they have no logs.
- `EMITTER`: the `address` of every log of the transaction.
- `TOPIC`, known events: for a log whose `topic0` is the keccak256 of one of the signatures
  below, the listed topics are addresses, taken whatever their value (so vanity addresses
  like Seaport's, with seven leading zero bytes, are not lost):

  | Event signature | Address topics |
  |---|---|
  | `Transfer(address,address,uint256)` (ERC-20 and ERC-721) | 1, 2 |
  | `Approval(address,address,uint256)` | 1, 2 |
  | `ApprovalForAll(address,address,bool)` | 1, 2 |
  | `TransferSingle(address,address,address,uint256,uint256)` | 1, 2, 3 |
  | `TransferBatch(address,address,address,uint256[],uint256[])` | 1, 2, 3 |
  | `Deposit(address,uint256)`, `Withdrawal(address,uint256)` (WETH) | 1 |

  A listed topic whose first 12 bytes are not zero is not an address and is skipped.
- `TOPIC`, everything else: every address-shaped topic the known-event rule did not
  classify, `topic0` included (an anonymous event can carry an address there).
- `WORD`:
  - the words of the transaction's input at offset 4: after the selector of a call, and in
    the init code of a creation (as TrueBlocks does; init code mostly yields bytecode
    fragments that pass the heuristic, a known source of noise);
  - the words of every log's data at offset 0;
  - the code address of every EIP-7702 authorization, whatever its value;
  - with traces: the words of every trace's `action.input` at offset 4 and of its
    `result.output` at offset 0. This finds addresses that only occur inside the
    execution: recipients a multisig, router or bundler passes on, or an address a
    factory returns.
- `AUTH`: the authority of an entry in the authorization list of a type-4 transaction. An
  entry counts if `y_parity ∈ {0, 1}`, `0 < r < n`, `0 < s ≤ n/2` (`n` the secp256k1 group
  order) and `ecrecover(keccak256(0x05 ‖ rlp([chain_id, address, nonce])), y_parity, r, s)`
  returns an address; that address is the authority. Chain ID and nonce are not checked, and
  it does not matter whether the authorization took effect (deciding that needs state).
  EIP-7702 skips invalid entries instead of rejecting the transaction, so this exact rule
  keeps importers on different libraries in agreement.
- `BLOCK` (slot 0): the header's beneficiary, the beneficiary of every uncle, the address of
  every withdrawal, and with traces the `author` of every `reward` trace.
- `TRACE`, from Parity-style `trace_block` (Erigon, Reth, Anvil; not geth): for every trace
  of a transaction, `action.from` and `action.to` (every call frame, including the
  top-level one and delegate and static calls), the created contract (`result.address` of a
  `create`), and for a `suicide` the destroyed contract (`action.address`) and its
  beneficiary (`action.refundAddress`). Only importers with a tracing source set it; the
  slice's `roles` byte says which roles were detected.

  Traces cannot be verified without re-executing the block, so an importer that sets
  `TRACE` checks the `trace_block` answer and **rejects the block** (publishes nothing for
  it, and stops) unless:
  - every trace has type `call`, `create`, `suicide` or `reward`, and this block's
    `blockHash`;
  - a `reward` names no transaction, and its `author` is the beneficiary of the block or
    of an uncle;
  - every other trace names an existing transaction by `transactionPosition` and
    `transactionHash`;
  - every transaction has exactly one root trace (empty `traceAddress`), which rules out
    an empty or partial answer.

  A trace of another type therefore stops the import instead of being skipped: skipping it
  is exactly how two importers would end up with different slices.

A wallet that only wants transfers it took part in filters on `FROM | TO | TOPIC | TRACE |
BLOCK | AUTH`. `WORD` keeps the index as complete as TrueBlocks' and gives spam filters
something to work with. Without traces it was 1–10 % of all appearances (measured with the
earlier, stricter heuristic); the share with traces and the TrueBlocks heuristic is still
to be measured with `addr-stats` on a tracing node (#8).

The appearances of one address at one (block, slot) are merged into one entry whose role
is the OR of all roles.

## Parameters

Namespace `addr1` fixes these values. A change of any of them is a new namespace.

| Name | Value | Why |
|---|---|---|
| `U` (unit length) | 64 blocks | ≈ the finality delay; small units keep the index fresh |
| `L_max` (top level) | 16 | top slices of 2^16 units = 4,194,304 blocks are never merged again |
| bucket target `T` | 2816 bytes | average bucket payload; about 1 bucket in 3,000 exceeds a chunk and is paged |
| spill threshold | 256 bytes | inline entries of one address above this go to a spill file |
| fingerprint | 8 bytes | collision handling below |

## Slices and covers

Slice `(L, i)` at level `L ≤ L_max` covers units `[i·2^L, (i+1)·2^L)`, that is blocks
`[i·2^L·U, (i+1)·2^L·U)`.

**Cover.** The cover of `[0, N)` units is every complete top-level slice, followed by one
slice for every set bit of `N` below `L_max`, from the highest bit down:

```text
start = 0
for j in 0 .. (N >> L_max):               # complete top-level slices, oldest first
    slice (L_max, j)
    start += 2^L_max
for L from L_max − 1 down to 0:
    if N has bit L set:
        slice (L, start >> L)              # covers units [start, start + 2^L)
        start += 2^L
```

Example (cap not reached): `N = 13 = 0b1101` → `(3, 0)` units 0–7, `(2, 2)` units 8–11,
`(0, 12)` unit 12.

```text
units 0 ──────────────────────────────────────▶ N
      [          (3,0)          ][   (2,2)  ][(0,12)]
newest-first: (0,12) → (2,2) → (3,0)      oldest-first: the reverse
```

Mainnet at block 26,428,827 is `N = 412,950` units: 6 top-level slices plus 7 lower ones,
13 in all. A cover never has more than `(N >> 16) + 16` slices: 22 until block 29,360,128
(about the end of 2027), and one more every 1.6 years after that.

Below `L_max`, every slice in a cover has an **even** index `i`: `start >> L` has bit 0
clear, because bit `L` of `start` is still unset when the slice is chosen. A lower slice
with an odd index would be complete at the same moment as its parent
`(L + 1, (i − 1) / 2)`, so it never appears in any cover and is never written. Top-level
slices are written for every index.

**Writing: one slice per unit.** When unit `N − 1` is complete and finalized, the cover of
`[0, N)` differs from the cover of `[0, N − 1)` by exactly one new slice: with
`L = min(trailing zero bits of N, L_max)`, it is `(L, (N >> L) − 1)`, covering units
`[N − 2^L, N)`. The importer writes that slice (buckets first, the `SliceRecord` last), then
publishes the head (below) with `N`. A lower level `L` gets a slice for one unit in
`2^(L+1)`. Over its life a block is uploaded about 9 times: in about half of levels 0–15,
and once at level 16.

**Large merges in the background.** The new slice cannot be prepared ahead (its buckets
depend on the last unit), and it can be large: about 60 K chunks at level 8, 1 M at level 12,
15.5 M at level 16 (extrapolated from the measurements). Publishing the head only after
such a slice is uploaded would leave the index hours or days behind. So the importer may
**defer** the merge:

1. It writes the level-0 slice `(0, N − 1)` of the last unit. This is the only case in which
   a slice with an odd index is written.
2. It publishes the head for `N` with `(L, (N >> L) − 1)` in its `pending` list.
3. It builds and uploads the merged slice in the background, and drops it from `pending`
   with the first head update after the slice is complete.

A client replaces every pending slice `(L, i)` in its cover by the slices it merges, all of
which exist: the cover of units `[i·2^L, (i+1)·2^L − 1)` (even indices, written when they
completed) followed by `(0, (i+1)·2^L − 1)` from step 1. That is `L + 1` slices, so a lookup
reads up to 16 more buckets while a top-level merge runs. If that expansion contains another
pending slice (a merge was deferred before the previous one finished), it is expanded the
same way.

The importer needs a sustained upload rate of only about 4 chunks per second
(≈ 10 M chunks per month at today's activity, see *Full history and cost*), but merges come
in bursts: a top-level merge at 200 chunks per second takes about a day. The block-store
importer in this repository uploads one chunk per request, sequentially; the index importer
must upload in parallel.

**Initial import (backfill).** Starting at `N0` complete units, the importer writes only
what a cover can still reference: the top-level slices `(L_max, j)` for `j < N0 >> L_max`
and the lower slices of the cover of `[0, N0)`. Every other lower slice of the past would be
dead on arrival and is skipped. Then it publishes head update 0 with `N0`, writes the same
record to the origin identifier (below), and from then on one slice and one head update per
unit.

**Starting at a later block.** An index need not start at genesis. It can cover the units
from `S` on, where `S` is the unit of a chosen block: a recent history is ready in hours
instead of weeks, and an importer without the old blocks (a node that expired them) can
still publish one. The slice grid stays the one from genesis, with the same identifiers,
covers and merges; only the slices that hold nothing are left out:

- A slice `(L, i)` exists only if it ends after `S`, and it holds the units
  `[max(i·2^L, S), (i+1)·2^L)`. Its `SliceRecord` says so in `first_block`
  (`max(i·2^L, S)·U`), and its entries' block deltas start from that block.
- Every head states `S` (as `first_block = S·U`), the same in every update; the origin
  does too. `N0 > S`.
- The backfill writes the slices of the cover of `[0, N0)` that end after `S`; every later
  unit and merge proceeds as above. The importer records `S` and `N0` before the first
  upload, and a backfill that fails is repeated with exactly those: the slice the start
  falls into depends on `S`, its identifiers do not, and they are written once. A merge the start falls into holds its units from `S`
  on, so it still holds exactly the entries of the slices it replaces.
- A client reads what an incremental sync from unit `S` would read: the slices of the
  resolved cover that end after `S`. It reports the blocks before `S·U` as **not covered**,
  never as empty, and it compares with the chain only from `S·U` on.
- The start is fixed for the life of the index. An earlier start cannot be added later (the
  slices around the old start are written and immutable): it is a new index under another
  key, which can run beside the first and replace it once it has caught up.

`S = 0` is an index from genesis; every rule above then reduces to the ones before.

**Incremental sync.** A client that has read everything up to unit `M` takes the cover of
`[0, N)` and reads only its slices that end after unit `M`. A slice that starts before `M`
is read whole and the entries before unit `M` are dropped. A client `d` units behind reads up
to `⌈log2(d + 1)⌉` such slices, each a descriptor and a bucket it has not seen (about 7 after
a day, 10 after a week, 12 after a month; about half that plus one on average). Results the
client keeps per block range stay valid, because a merged slice holds exactly the entries
of the slices it replaces.

**Lifetime.** A lower slice `(L, i)` is in the cover of the head while `N` is in
`[(i + 1)·2^L, (i + 2)·2^L)`, i.e. for `2^L·U` blocks, plus a grace period for clients that
still hold an older head, and longer while a pending merge's expansion uses it. Top-level
slices are permanent. Only the slices in the current cover (and expansions of pending
merges) must stay, and together they hold one copy of the history.

The lower levels are not small, though. On average the live lower slices hold half a
top-level slice of data (2^15 units), just before a top slice is written almost a whole one,
and postage stamps cannot be freed chunk by chunk: a batch pays for every chunk written into
it until the batch expires. The importer therefore stamps the lower levels separately from
the permanent top level, grouped by lifetime (level `L` lives `2^L·U` blocks; level 15 about
291 days) so that each batch expires soon after its slices are dead. A mutable batch does
not help here: it evicts the oldest stamps first, and the oldest live lower slices are the
long-lived ones. See *Full history and cost* for the size.

The cap is what keeps the history from being rewritten: without it, the whole history
would be merged into one new top slice every time `N` reaches a power of two, briefly
doubling the stored data.

## Identifiers

As in the block store (`src/layout.rs`), every SOC identifier is the keccak256 of an ASCII
string; `<chain>` is the decimal chain ID; all numbers in identifiers are decimal.

| Object | Identifier | Payload |
|---|---|---|
| Slice descriptor | `keccak256("eth:<chain>:addr1:slice:<L>:<i>")` | `SliceRecord` |
| Bucket | `keccak256("eth:<chain>:addr1:bucket:<L>:<i>:<k>")` | `BucketRecord` |
| Head (feed update `n`) | `keccak256(topic ‖ n as u64 BE)`, `topic = keccak256("eth:<chain>:addr1:head")` | `HeadRecord` |
| Origin | `keccak256("eth:<chain>:addr1:origin")` | `HeadRecord`: a copy of head update 0 |
| Spill file | Swarm file (`/bytes`), referenced from a bucket | spill pages |
| Bucket pages | Swarm file (`/bytes`), referenced from a paged bucket | bucket pages |

The head identifiers follow Bee's sequence-feed scheme, but the design does not rely on
Bee's feed lookup: that lookup finds the last update by asking for indices until one is
missing, so it always pays at least one retrieval timeout and grows with `n` (about 41,000
updates a year). Every unit gets exactly one head update, also while the importer catches
up after downtime, so `N − n` is constant: `N0`, which a client reads once from the origin
record. A client without a cached head starts from the origin like a returning one.

A client avoids asking for an update that does not exist yet (a miss is a timeout), and
bounds the misses it cannot avoid. It caches the last head it read, or the origin
(`n`, `N`, `last_block_timestamp`):

- **Best:** if the client knows the latest finalized block number `F` from elsewhere (a
  beacon light client, an RPC), the newest update that can exist is `⌊(F + 1) / U⌋ − N0` (update `n` announces
  `N0 + n` units).
  A miss then means the importer is behind.
- **Otherwise, from the time.** Twelve seconds is the slot time; a missed slot has no block,
  so time only gives an *upper* bound on the blocks produced. The client aims low: it does
  not ask for `n + 1` before `last_block_timestamp + 2·U·12 s` (one more unit, plus the
  finality delay), and after a longer pause it targets
  `n + ⌊0.95 · (elapsed seconds − U·12) / (U·12)⌋`, 5 % below the slot count.
- **On a miss** it halves the jump towards `n` and tries again, so a jump of `d` updates
  costs at most `log2(d)` misses. Once the jump is down to `n + 1`, further misses back off
  exponentially (2, 4, 8 … minutes, at most 30). The first update it does get gives the
  exact `units`, and with it the right index for the next step.

All of this runs in the background; the client keeps showing what it has.

Unlike the block store, the identifiers are **not** namespaced by a trust anchor: the index
grows incrementally. Each slice binds to the chain through the hash of its last block.

## Record formats

All records start with `MAGIC ‖ VERSION ‖ kind` as in `src/layout.rs` (`"E2S"`, `1`).
Integers are big-endian unless they are varints. A **varint** is unsigned LEB128 (7 bits
per byte, low group first, high bit set on all but the last byte), always in its shortest
form.

Encodings are canonical: the same appearances must give byte-identical records, spill files
and therefore Swarm references, or independent importers cannot be compared. Besides
shortest varints that means the orderings given below, greedy spill pages, zero padding, and
spill files uploaded with Bee's default chunker, unencrypted and without erasure coding
(redundancy level 0), since encryption and redundancy change the reference. Bee 2.8.3
erasure-codes `POST /bytes` unless the request sets `Swarm-Redundancy-Level: 0`, so the
importer sets it. With it, the reference can be computed before uploading
(`src/swarm/file.rs`, checked against Bee at every level boundary), which is how a bucket
names its spill and pages files.

### `SliceRecord` (kind 4), 111 bytes

| Field | Type | Meaning |
|---|---|---|
| header | 5 bytes | `"E2S" ‖ 0x01 ‖ 0x04` |
| `unit_blocks` | u32 | `U` (64) |
| `level` | u8 | `L` |
| `index` | u32 | `i` |
| `first_block` | u64 | the first block the slice holds: `i·2^L·U`, or `S·U` for the slice the start of an index from unit `S` falls into (*Starting at a later block*); always a unit boundary within the slice |
| `last_block` | u64 | `(i+1)·2^L·U − 1` |
| `last_block_hash` | 32 bytes | hash of `last_block` |
| `bucket_count` | u32 | number of buckets `B ≥ 1`; all of `0 … B−1` exist |
| `addresses` | u32 | distinct addresses in the slice |
| `appearances` | u64 | entries in the slice |
| `buckets_root` | 32 bytes | Merkle root over `keccak256(bucket payload)` for `k = 0 … B−1` |
| `roles` | u8 | the role bits the importer detects for this slice (e.g. no `TRACE` without a tracing source) |

`buckets_root` uses the accumulator's keccak256 tree (`src/accumulator.rs`, leaves
zero-padded to the next power of two). It is there for reproducibility: an auditor who
rebuilds a slice can compare it by this root alone. A client cannot check a bucket against
it (see *Verification*).

`B = max(1, ⌈(sum of address-record bytes) / T⌉)`, the records encoded with the 256-byte
spill threshold. A slice without appearances (possible on a dev chain) still has one, empty,
bucket.

### `BucketRecord` (kind 5)

| Field | Type | Meaning |
|---|---|---|
| header | 5 bytes | `"E2S" ‖ 0x01 ‖ 0x05` |
| `level` | u8 | `L` |
| `index` | u32 | `i` |
| `bucket` | u32 | `k` |
| `paged` | u8 | 0: the records follow; 1: they are in a pages file |
| `count` | u32 | number of address records |
| records | | if not paged: `count` address records, ascending by (fingerprint, address) |
| `pages` | u32 | if paged: number of pages |
| `pages_ref` | 32 bytes | if paged: Swarm reference of the bucket pages file |

Address `A` belongs to bucket `k = (u32(h[0..4]) · B) >> 32` and has fingerprint
`fp = h[4..12]`, where `h = keccak256` of the 20 address bytes, `u32` reads big-endian,
and `B` is the slice's `bucket_count`. The multiply-shift spreads addresses evenly over any
`B`, so `B` does not have to be a power of two and buckets average `T` bytes.

Address record:

| Field | Type | Meaning |
|---|---|---|
| `fp` | 8 bytes | fingerprint |
| `n_spill` | varint | `n << 1 | spilled`, where `n` is the number of entries |
| entries | | if not spilled: `n` entries, ascending by (block, slot) |
| `spill_ref` | 32 bytes | if spilled: Swarm reference of the spill file |

Entry: `varint(block_delta) ‖ varint(slot) ‖ role (u8)`. `block_delta` of the first entry
is relative to `first_block` of the slice (its `SliceRecord`'s, so a bucket is decoded with
its descriptor), then to the block of the previous entry (0 for another slot in the same
block).

An address whose inline entries would take more than 256 bytes is **spilled**: its record
holds only `n` and `spill_ref`, and its entries go to a spill file of their own.

Fingerprints are not unique: by chance two addresses in a bucket can share 64 bits of their
hash, and on purpose it takes a 96-bit collision of `h[0..12]` (about 2^48 hashes). Records
are therefore ordered by fingerprint and then by the 20 address bytes, which the importer
knows although the record does not carry them. A client reads every record with its
fingerprint, and verification (below) drops entries that are not about its address.

**Paged buckets.** A bucket whose inline payload, header included, would exceed 4096 bytes is
stored paged: the bucket SOC holds only the header, `count`, `pages` and `pages_ref`, and the
records go to a pages file. Pages are 4096 bytes, `count (u16) ‖ records`, filled greedily in
record order, zero-padded except the last; page `p` is the `p`-th data chunk of the file. A
client binary-searches the pages by the fingerprint of their first record, paying a path
through the file's tree per probe.

Paging is rare by chance (0–0.03 % of buckets at `T = 2816`, measured) but can be forced:
addresses that share `h[0..4]` share a bucket for every `B`, and 340 single-entry records
(12 bytes each) already overflow a chunk. Grinding 340 such values costs about 1.5 × 10^12
hashes, and a single transaction can put them on chain as `WORD`s. Paging keeps every slice
writable in that case, every bucket SOC a single chunk (which `src/swarm/soc.rs` requires),
and the cost to the affected addresses' lookups a few extra chunks. A per-slice salt in the
bucket hash would prevent the grinding, but it would also give a client's address a
different bucket set in every slice, so whoever sees the whole lookup could intersect them
down to the address itself.

### Spill file

A Swarm file made of 4096-byte **pages**. Page `p` is exactly the `p`-th data chunk of the
file, so a client fetches any page on its own:

| Field | Type | Meaning |
|---|---|---|
| `count` | u16 | entries in this page |
| `first_block` | u32 | block of the first entry |
| entries | | as in the bucket; the first `block_delta` is relative to `first_block` (so 0) |
| padding | | zero bytes to 4096, except in the last page, which has none |

Pages are filled greedily: an entry goes into the current page if it fits, else it starts
the next page. No page is empty.

The client learns the number of pages from the file's span (`⌈span / 4096⌉`), reads the
last page for the newest entries, and binary-searches pages by `first_block` for a block
range. `u32` block numbers last until block 2^32, more than 1600 years of 12-second slots.

### `HeadRecord` (kind 6), 70 + 5·p bytes

| Field | Type | Meaning |
|---|---|---|
| header | 5 bytes | `"E2S" ‖ 0x01 ‖ 0x06` |
| `first_block` | u64 | `S·U`, the first block the index covers (0 for an index from genesis); the same in every update and the origin, `S < N` |
| `units` | u64 | `N`, the number of complete units, at most 2^26 (spill pages store block numbers as u32) |
| `last_block` | u64 | `N·U − 1` |
| `last_block_hash` | 32 bytes | hash of `last_block` |
| `last_block_timestamp` | u64 | timestamp of `last_block` (seconds) |
| `pending_count` | u8 | `p`, the number of deferred merges |
| `pending` | `p` × (u8 level ‖ u32 index) | the deferred merged slices, strictly ascending by `(i·2^L, L)`, the grid's first unit and the level (for an index from unit `S`, not the unit the slice holds from): an outer merge deferred while an inner one is pending starts at the same unit; each ends after unit `S` |

## Client lookup

Newest first, for address `A`:

1. Read the head as described under *Identifiers* (from the cached head or the origin) → `S`,
   `N` and the pending merges.
2. Compute the cover of `[0, N)`, expand the pending slices, and take the slices that end
   after unit `S`, or after unit `M ≥ S` for an incremental sync. For `S > 0`, show the
   blocks before `S·U` as not covered.
3. For each slice, newest first: read the `SliceRecord` (immutable, cached forever), compute
   `k` and `fp`, read bucket `k`, and scan it for `fp` (records have variable length, so the
   2.8 KB are read front to back). For a paged bucket, binary-search its pages.
4. For a spilled address, walk the spill file's tree from its root chunk to the last page,
   then earlier pages as the user scrolls. Each tree level is one chunk and one round
   trip: root and page for up to 128 pages, one intermediate level more up to 16,384 pages
   (≈ 16 million entries). A binary search over pages costs such a path per probe.
5. Stop when the screen is full; continue with older slices on demand.

Descriptors of all slices in the cover can be fetched in parallel, then all buckets in
parallel: two round trips for a cold lookup, one when the descriptors are cached.

Oldest first is the same in the opposite order.

## Verification

- **Most entries can be checked.** Fetch the header of the block from the block store
  (verified against the trust anchor), then the evidence for the role:
  - `FROM`, `TO`, `WORD` from the input or init code, `AUTH`: the transaction (sender and
    authorities by signature recovery; a created contract's address from sender and
    nonce).
  - `EMITTER`, `TOPIC`, log `WORD`: the receipt.
  - `BLOCK` (slot 0): the header itself (beneficiary) or the body (withdrawals, uncles),
    checked against the header's roots. A pre-merge reward `author` from a trace is the
    beneficiary of the block or an uncle, so the same evidence applies.
  - `TRACE`, and a `WORD` found only in a trace's input or output: cannot be checked from
    the block. Internal calls are not recorded in the transaction or the receipt; checking
    one needs re-execution with the state, or an execution proof. A client trusts the
    importer for these entries, or compares importers. A client cannot tell from the
    entry whether a `WORD` came from a trace; if it finds the address in neither the
    transaction nor the receipt, it treats the entry as trace-derived.
- **Checking one transaction is cheap only if the block store supports it.** Today's block
  store, like devp2p, delivers only whole bodies (164 KB on average at the tip), and the
  client recomputes `transactionsRoot` from all of them. To read one transaction, the block
  store must add receipts, a per-block table of transaction offsets in the body file, and
  the internal nodes of the transaction and receipt tries for proofs against
  `transactionsRoot` / `receiptsRoot`. The formats belong in `soc-block-store.md`; the
  sizes are under *Measurements*.
- **Completeness cannot be proved to the client.** A bucket is signed by the importer but
  says nothing verifiable about what it leaves out. Proving absence needs a commitment to
  "all addresses in this block" from a source the client already trusts, and block headers
  have none (`logsBloom` is lossy, nearly saturated on current blocks, and over the whole
  history about as large as TrueBlocks' blooms). What the design offers instead:
  - *Reproducibility.* Slices are deterministic: anyone with a node can rebuild one and
    compare `buckets_root`. TrueBlocks offers the same.
  - *Attribution.* A signed bucket that lacks an appearance the chain shows is permanent
    evidence against its owner.
  - *Independent importers.* If several parties build the index under their own keys, a
    client first compares their `SliceRecord`s for the slice. They must match in
    `bucket_count`, `roles`, `addresses`, `appearances` and `buckets_root`. Importers that detect different roles (one has
    traces, one has not) are not comparable. If the descriptors match, the client reads
    the same bucket `k` from each importer and compares the bucket payloads (or their
    keccak256). If they agree, the list is complete unless all of them omit the same
    entries. Comparing only `buckets_root` is not enough: a bucket carries no proof against
    that root, so a dishonest importer could copy an honest root and serve a different
    bucket. This costs one more descriptor and bucket per slice and importer, and it
    requires the detection rules and canonical encodings above to be followed exactly.
  - Real proofs need an index committed by the protocol itself, or zero-knowledge proofs
    that a slice was built correctly from the blocks. Both are out of scope.
- **An index from a later block claims nothing before it.** Its head states the start, and
  every descriptor must start where that start makes it (`addr-verify` and `addr-lookup`
  check this). Importers with different starts are compared on the slices that hold the
  same units in both: those that begin at or after both starts.

## Measurements

**These figures predate the current detection rules.** They were measured with the earlier
rule set: the 2^128 word bound, no init code, no `topic0`, no traces. The current rules (the
TrueBlocks superset) find more appearances, so the totals, sizes and costs below are lower
bounds until they are measured again with `addr-stats`, with traces once a tracing node is
available.

**Reproducing them:** `ethswarm addr-stats --level L --at <block> [--at …] [--traces]`
builds every slice of levels `0..=L` in the level-`L` window around each block with the real
encoder and prints one table per window: the per-level columns below (addresses,
appearances, bytes per appearance, buckets, paged buckets, spilled addresses, chunks
including every chunk of the spill and pages files, chunks per 1000 appearances, largest
bucket) and each window's `WORD`, `TRACE` and `TRACE`-only shares. A window is the aligned
slice that contains the `--at` block; it does not start there. The `Level 0` row of a
`--level 5` window (2,048 blocks, the default) gives a per-era row below, `--level 8` the
slice-length table.

Measured on 2026-11-16 against mainnet (the zbox geth node: full history, no traces, so no
`TRACE` role) with throwaway scripts that encode the sampled appearances in the record formats above.
Simplifications: the address-shaped test was applied to all topics (no known-event rule),
`AUTH` and uncle beneficiaries were not detected, a created contract's address was taken from
the RPC receipt, the bucket header was counted as 18 bytes (19 with `paged`), and an
overflowing bucket was counted as a two-chunk file (as a paged bucket it is three: the
bucket SOC, the file root and one page). Each changes the totals by a fraction of a
percent. Bucket assignment used sha256 instead of keccak256, which gives the
same size distribution for a uniform hash. `addr-stats` replaces the scripts.

Sampled: 2,048 consecutive blocks at each of 1M, 4M, …, 22M; the 16,384 blocks from
26,400,000; one block in 5,000 over the whole chain for block sizes; 8 consecutive blocks in
100,000 for receipt sizes.

### Per era (64-block slices, `T = 2816`)

| From block | Txs / block | Appearances / block | `WORD` only | Addresses / slice | Bytes / appearance | Buckets / slice | Chunks / slice |
|---|---|---|---|---|---|---|---|
| 1,000,000 | 3 | 8 | 1 % | 172 | 6.37 | 1.4 | 2.5 |
| 4,000,000 | 61 | 142 | 5 % | 3,272 | 6.42 | 18 | 31 |
| 7,000,000 | 105 | 284 | 3 % | 7,413 | 6.85 | 41 | 61 |
| 10,000,000 | 115 | 379 | 8 % | 11,400 | 7.45 | 60 | 82 |
| 13,000,000 | 186 | 608 | 9 % | 13,735 | 6.69 | 77 | 130 |
| 16,000,000 | 116 | 418 | 7 % | 11,022 | 6.84 | 59 | 84 |
| 19,000,000 | 165 | 599 | 10 % | 14,620 | 6.70 | 80 | 121 |
| 22,000,000 | 147 | 641 | 6 % | 17,243 | 7.01 | 94 | 128 |
| 26,400,000 | 289 | 1,505 | 6 % | 42,366 | 7.33 | 232 | 315 |

Bytes per appearance include spill files. Chunks per slice count buckets, overflow chunks and
spill-file chunks (one root chunk per spill file).

### Slice length (16,384 blocks from 26,400,000, `T = 2816`)

| Slice | Level | Addresses | Appearances | Bytes / appearance | Buckets | Spilled addresses | Chunks | Chunks / 1000 appearances | Buckets > 4096 B |
|---|---|---|---|---|---|---|---|---|---|
| 64 blocks | 0 | 42,366 | 96,332 | 7.33 | 232 | 65 | 315 | 3.27 | 0 % |
| 256 | 2 | 131,369 | 385,330 | 6.65 | 778 | 369 | 1,255 | 3.26 | 0.008 % |
| 1,024 | 4 | 412,775 | 1,541,318 | 6.15 | 2,657 | 1,384 | 4,578 | 2.97 | 0.014 % |
| 4,096 | 6 | 1,325,267 | 6,165,273 | 5.79 | 9,124 | 5,016 | 16,649 | 2.70 | 0.03 % |
| 16,384 | 8 | 3,774,184 | 24,661,091 | 5.28 | 29,680 | 19,292 | 60,366 | 2.45 | 0.03 % |

Longer slices are denser: the per-address overhead (fingerprint and count) is shared by more
entries. Top-level slices are 256 times longer than the longest one sampled, so the
full-history figures below, which use the 16,384-block values, should be on the high side.

`T` was chosen from the same sample: `T = 3072` saves 4 % of the chunks but overflows 0.4 %
of the buckets of the longest slice; `T = 2560` costs 5 % more chunks than `T = 2816` and
leaves one overflow in 32,648 buckets.

### Full history and cost

| | Value |
|---|---|
| Appearances, genesis to block 26,428,827 (linear between the samples) | ≈ 13.0 billion |
| One copy of the index (5.28 B and 2.45 chunks per 1000 appearances) | ≈ 69 GB, ≈ 32 M chunks |
| Growth at the current rate (1,505 appearances per block) | ≈ 0.8 M chunks per month |
| Postage batch | depth 26: 55.7 M chunks effective, about 2 years of headroom |
| Postage price on 2026-11-16 | 205,492 PLUR per chunk per Gnosis block (`lastPrice()` of the PostageStamp contract) |
| Monthly cost of the depth-26 batch | 2^26 × 205,492 × 518,400 blocks ≈ 715 BZZ ≈ $48 at $0.067 per BZZ |
| Lower levels, live | data of 2^15 units on average (≈ 2.1 M blocks × 1,505 × 3.0 per 1000 ≈ 10 M chunks), up to twice that just before a top slice is written |
| Lower levels, written | each block about 8 times: ≈ 8.5 M chunks per month |
| Lower levels, postage | stamps cover written chunks until their batch expires; with batches grouped by lifetime, about 20–30 M slots at any time, one more depth-25 batch: ≈ 357 BZZ ≈ $24 per month, up to twice that if lifetimes are matched poorly |
| **Total** | **≈ $72–96 per month** at today's prices |

Postage prices and the BZZ exchange rate move; the chunk counts are what to recompute
against. A lower `L_max` makes the lower levels cheaper (they hold up to one top slice) and
cold lookups more expensive (one bucket per top slice): `L_max = 14` would cut the lower
levels to a quarter and need 25 top slices today instead of 6.

### Client

| Operation | Data |
|---|---|
| Cold full-history lookup, typical address (13 slices today, at most 22 until late 2027) | 13–22 buckets of ≈ 2.8 KB + as many 111-byte descriptors: ≈ 40–65 KB |
| Incremental sync, `d` units behind | up to `⌈log2(d + 1)⌉` new slices, each a descriptor and a bucket, in two round trips: after a day up to 7 (≈ 10–20 KB), after a month up to 12 |
| While a merge is pending | up to `L + 1` buckets instead of one for the pending slice (up to 17 for a top-level merge) |
| Spilled address | per slice, the path from the spill file's root to its last page (2–3 chunks) for the ≈ 1,000 newest entries |
| Storage on the phone | the results for its own addresses; descriptors are cached forever |

### Verification data (block store, per block)

| From block | Tx-trie inner nodes | Tx proof, avg / max | Body | Receipts |
|---|---|---|---|---|
| 1,000,000 | 0.1 KB | 126 B / 1.0 KB | 1.1 KB | 1.1 KB |
| 7,000,000 | 3.8 KB | 733 B / 1.2 KB | 19 KB | 44 KB |
| 13,000,000 | 6.7 KB | 860 B / 1.7 KB | 84 KB | 101 KB |
| 19,000,000 | 5.9 KB | 869 B / 1.6 KB | 154 KB | 106 KB |
| tip | 12.4 KB | 1.08 KB / 1.6 KB | 164 KB | 192 KB |

Trie sizes were computed from the trie shape (keys `rlp(index)`, 32-byte child references),
not from the node. Receipts are canonical RLP including the 256-byte logs bloom, which is
about 41 % of all receipt bytes and can be recomputed from the logs.

With offsets and trie nodes in the block store, checking one current transaction reads the
header SOC (≈ 1.5 KB), the body file's root chunk (≈ 1.3 KB), the one or two data chunks
holding the transaction, and one or two chunks of offsets and trie nodes: **≈ 11–23 KB**,
and about as much again for its receipt. Without them it reads the whole body, and the
receipts on top for log roles.

## Open questions

- **Relation source.** Without traces (`TRACE`) internal ETH transfers are missing. Our geth
  node has full history but not archive state, so it cannot trace historical blocks.
  Options: TrueBlocks' published index for history plus a tracing client (Erigon, Reth) for
  new blocks.
- **Where the blocks come from.** The index does not care: an entry is (block, slot). Since
  2025 execution clients can drop pre-merge bodies and receipts, and many nodes do, while
  post-merge history is still served over devp2p; eth/69 announces each peer's earliest available block, so a
  client can choose peers without probing. A split worth evaluating: pre-merge blocks
  (including receipts) on Swarm, post-merge **headers and their proofs** on Swarm (≈ 1.15
  chunks per block), post-merge bodies and receipts from devp2p, checked against those
  headers. devp2p delivers whole blocks only, so this suits a wallet that checks a
  transaction when it is opened. If post-merge history expiry follows, Swarm takes over
  block ranges as they leave the devp2p window.
- **Head of chain.** The index runs up to about 32 minutes behind. A wallet that needs "paid just
  now" uses RPC or a light client for the last blocks.
- **Privacy default.** `T` sets the k-anonymity of a single bucket request, but a whole
  lookup can be intersected across slices (see *Design constraints*). A wallet may fetch
  decoy buckets or use its own Bee node; how much that buys is open.
- **Block store.** Receipts, transaction offsets and trie nodes (above); moving the header
  accumulator to a Merkle Mountain Range (the same doubling scheme) would remove the
  "extending the range changes the anchor" limitation.
- **Tracer-dependent edge cases.** `TRACE` and trace `WORD`s are defined by an RPC
  format, not by the EVM, and Parity-style tracers differ at the edges. For example, a
  reverted frame's revert data may or may not be in `result.output`. A call to a precompile
  may get a frame only at top level or with value, and `ecrecover`'s output is an
  address-shaped word. Identical slices across importers need these cases written into the
  rules, or the tracer named as part of them. To be settled when the detection is compared
  against TrueBlocks on an Erigon node.
- **Genesis allocations.** TrueBlocks indexes the prefunded addresses of the genesis
  state at block 0 (about 8,900 on mainnet). Adding them needs the allocation list as an
  input (it is not in any block), and a convention for its slots.
- **Adding traces later.** A slice's `roles` byte says what was detected; a slice rebuilt
  with traces would have to be published under a new namespace.
