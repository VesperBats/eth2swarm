# Completeness proofs for the address index (design sketch)

Status: **design sketch, not implemented.** Costs are **orders of magnitude**, not
measurements; the measurements that would replace them are under *Open questions*.

The address index ([address-index.md](address-index.md)) lets a client check that every
entry it lists is real. It cannot let a client check that **nothing is missing**: a bucket
is signed by the importer but says nothing verifiable about what it leaves out
(*Verification* in address-index.md). This note describes how that gap could be closed, in
steps that each stand on their own:
1. a public commitment, which gives every reader the same view (*one view*);
2. fraud proofs, which make an omission cost the importer (*accountability*);
3. zero-knowledge validity proofs, which prove a slice complete (*completeness*).

It also covers what the protocol itself might add (EIP-7745), and what stays out of reach
(`TRACE`, availability).

## Why completeness matters

An address history, or an `eth_getLogs` answer, means "these are **all** the entries". An
importer, a seed publisher or an RPC provider that leaves entries out is undetectable from
the answer alone. What that does depends on the reader:
- **A wallet's history** shows fewer transfers than happened.
- **A shielded pool** (RAILGUN, for example) shows the wrong balance:
  - a withheld commitment or note understates it, and a commitment withheld in the middle
    of the tree blocks spending;
  - a withheld nullifier overstates it, and the spend later reverts;
  - a replaced note ciphertext can show a payment that never happened.

  The publisher cannot steal funds: spending needs the user's keys and a valid zk proof.
- **Partitioning deanonymizes.** A source that serves different readers different views
  can recognize them later. RAILGUN's contract accepts every root its tree ever had, so a
  tree truncated at a point chosen per reader still yields a valid root. A reader's spend
  proofs then reference a root unique to that reader, and the spend, which is public,
  links to whoever downloaded that view.

None of this is specific to Swarm. Every wallet that reads logs from someone else, a public
RPC included, has the same completeness gap. A public RPC can also forge logs and sees
every query. Verifying every entry, as Myotis does, rules out forgery; what remains is the
gap this note is about.

## How Ethereum commits to logs today

- **`receiptsRoot`** in each header commits to all of that block's receipts, and so to all
  its logs and their order. A client that downloads every receipt of a range and checks it
  against the headers knows it has seen every log there. That is the only native
  guarantee. It costs the receipts of the whole range: about 200 KB per block at the tip,
  roughly 0.5 TB for a year.
- **`logsBloom`** is a lossy filter per block. A negative bloom proves a block has no
  match, but mainnet blooms have been nearly saturated for years, so they almost never
  rule a block out.
- **Internal calls (`TRACE`)** have no commitment at all. They exist only as a result of
  executing the block.
- **Old block hashes on chain:**
  - `BLOCKHASH` reaches the last 256 blocks, and the EIP-2935 history contract (since
    Pectra) the last 8,191, about 27 hours.
  - The EIP-4788 beacon block root reaches the last 8,191 slots. Through the beacon state,
    `historical_summaries` reach every block since Capella, and the frozen
    `historical_roots` the blocks from the Merge to Capella, each with a longer proof.
  - **No beacon state commits to pre-Merge blocks** (mainnet below 15,537,394). Those need
    another anchor, such as the pre-Merge historical hashes accumulator. Its root is fixed,
    so a contract could hard-code it, but none holds it today.

**EIP-7745 ("trustless log index")** proposes replacing `logsBloom` with a log-index root
in the header, so that absence becomes provable with small proofs. It is a draft and, as
far as this note knows, not scheduled for a fork. It indexes the log's emitter and topics,
not its data. If it lands, completeness for `EMITTER` and `TOPIC` comes from consensus;
the steps below still matter for `FROM`, `TO`, `WORD` (input and log data), `AUTH` and
`BLOCK`.

## Step 1: one view, through a public commitment

The importer already signs every head and slice. What it cannot do today is prove that it
gave everyone the same ones: Swarm serves whatever SOC a reader asks for, and nothing
stops an importer from signing two different heads for the same update and serving them
to different readers.

**The commitment:** per head update, or per slice, the importer posts on chain:
- the block range and the hash of its last block;
- the slice's `buckets_root` (already in every `SliceRecord`), or the hash of the
  `HeadRecord`.

**What the contract checks:** that the block hash is canonical, through `BLOCKHASH` or the
EIP-2935 history contract. Units are final 13–32 minutes after their last block, so posting
within 27 hours is easy. One commitment per range and key; a second one for the same range
is refused, or is itself evidence of equivocation.

**What the reader checks:** that the slice it read from Swarm matches the commitment
(`SliceRecord.buckets_root`), with a verified state read of the contract. In Myotis that
is an `eth_getProof` read against a verified state root.

**What it buys:** every reader sees the same index, with two conditions:
- **The latest commitment, from verified state.** A reader takes the newest committed head
  and refuses an older one. Otherwise an importer can still partition by serving chosen
  readers a valid but stale head, a shorter cover: the RAILGUN truncation again.
- **The uncommitted tail.** Heads newer than the last commitment are not covered. With one
  commitment a day, up to a day of recent units can still differ per reader; a reader
  either stops at the committed head or treats the tail as unverified.

It proves nothing about completeness.

**What it costs:** one transaction per commitment. Per head update (every unit, about 13
minutes) that is about 3,300 a month. Batching a day's heads in a Merkle tree, one root per
day, keeps it to one transaction a day. The contract can then check only the batch's last
block hash. The others are bound by the hash chain, so a reader checks a leaf's slices
against it with the headers it verifies anyway.

**A format detail:** a reader cannot check a bucket against `buckets_root` today, because
no Merkle path comes with the bucket (*`SliceRecord`*). Step 1 needs one: `⌈log₂ B⌉`
sibling hashes, at most 25 or so. They must be stored so that one path is a few chunks.
Every bucket's hash in one file would be `32·B` bytes, and B grows with the slice:
- **Level 0:** a few hundred buckets, so about 5–9 KB, two or three chunks.
- **Level 8:** about 35,000 buckets, so about 1.1 MB.
- **Top levels:** millions of buckets, so hundreds of MB.

So the hashes go in a chunked Swarm file, laid out by tree level, so that a path touches
one chunk per few levels (*Open questions*).

## Step 2: accountability, through fraud proofs

**The model:** optimistic, as in an optimistic rollup. The importer posts a bond with its
commitments. Anyone who finds an omission can claim the bond with a fraud proof, which
shows two things:
1. **The appearance exists,** with the header proved canonical and the evidence for the
   role:
   - `FROM`, `TO`, `WORD` (input), `AUTH`: the transaction, with its proof against
     `transactionsRoot`. Sender and authorities need signature recovery (the `ecrecover`
     precompile).
   - `EMITTER`, `TOPIC`, `WORD` (log data): the receipt, against `receiptsRoot`.
   - `BLOCK`: the header itself (beneficiary), the uncle headers against `ommersHash`, or
     the withdrawal against `withdrawalsRoot`.
2. **The committed slice lacks it:** the bucket the address hashes to, with its path to
   the committed `buckets_root`. Then, depending on the record:
   - **Inline:** the bucket's entries hold no entry for that (block, slot) under that
     address's fingerprint.
   - **Spilled or paged:** the entries are in a spill or pages file (`spill_ref`,
     `pages_ref`). The proof then needs that file's pages with their chunk-tree (BMT)
     proofs to the reference, and a parser for those formats on chain.

   **A limit:** a fingerprint collision lets another address's record cover the same
   (block, slot) under the same fingerprint, and such an omission cannot be proved. It is
   rare (8-byte fingerprints), but the dispute game must accept "not provable" as an
   outcome.

**Where proofs come from:** Myotis already checks every entry it shows against verified
receipts. A reader that also checks a sample of blocks in full (all their receipts, about
200 KB each) against the index finds systematic omissions. A targeted omission (one
address, one block) is found only by whoever looks at that block, typically the user it
affects.

**What it buys:** a dishonest importer loses its bond. A reader can tell an importer whose
slices stood unchallenged for long, with many readers checking, from one that was caught.
It is deterrence, not proof: an omission nobody looks for stays.

**What it costs:** gas only per dispute, but the dispute game is real work:
- **old headers:** a block older than 27 hours needs its header proved through the beacon
  state (`historical_summaries` since Capella, `historical_roots` back to the Merge). A
  pre-Merge block needs an anchor no contract holds today (*How Ethereum commits to logs
  today*), so until one does, omissions before the Merge cannot be challenged on chain;
- **checking on chain:** the contract has to parse the bucket format and the receipt
  trie;
- **timing:** a challenge window, and a bond large enough to matter.

The address index's `TRACE` entries cannot be challenged this way: nothing on chain
commits to internal calls.

## Step 3: completeness, through zero-knowledge validity proofs

**The statement**, for one unit slice of 64 blocks, with the public inputs `first_block`,
*P* (the hash before it) and *H*:

> There are headers *h_a … h_b*, with *h_a* numbered `first_block` and its `parentHash`
> equal to *P*, each hashing to the next one's `parentHash`, and *h_b* hashing to *H*. There
> are bodies (transactions, uncle headers, withdrawals) and receipts matching their
> `transactionsRoot`, `ommersHash`, `withdrawalsRoot` and `receiptsRoot`. And the slice's
> canonical bytes (`SliceRecord`, buckets, spill and pages files), summarized by the public
> output `buckets_root`, are exactly *F*(those headers, bodies and receipts).

*F* is the detection rules (*Detection rules* in address-index.md), including signature
recovery for `FROM` and `AUTH`, and the canonical encoding (*Record formats*).

**Only for an index without `TRACE`.** F cannot produce `TRACE` entries, nor the `TRACE`
bit on an entry it shares with another role, from these inputs. So the proven index is one
built without traces, as the month test index was. `TRACE` would be a separate index
(other slices, another root) on the importer's word, or proven with a zkEVM (below). A
reader then combines a proven index with an unproven `TRACE` index, and marks the `TRACE`
entries as resting on the importer. Both are deterministic, and the reader conformance vectors
(`testdata/addr1-reader-vectors.txt`) pin the encoding byte for byte. That makes a
validity proof possible at all: for a given range there is exactly one correct slice.

**Recursion along the slice tree.** A merged slice's proof verifies its two children's
proofs and the merge, rather than repeating them, and checks that they chain: the left
child's *H* is the right child's *P*, and the first blocks follow. A head's cover (one
slice per level) then carries proofs for every block from the index's start to its last
unit. "Complete up to block N" becomes one small proof per slice of the cover. A client
checks each proof in milliseconds, and that the cover's slices chain (each slice's *P* is
the previous slice's *H*). It verifies the last slice's *H* through its own anchors: the
light client, or the checkpoint table.

**No chain needed.** The proof can sit on Swarm next to the slice, and a client verifies it
locally. Verifying it on chain once (a Groth16 or PLONK proof costs a few hundred thousand
gas, and a STARK can be wrapped into one) and storing the result adds Step 1's single view
to it. It also lets readers that cannot verify proofs read a verified flag instead.
A wrong slice is already impossible without it, since F has one output per range. A stale
one is not: a valid but older head, with a shorter cover, still proves out. Only a
commitment the reader reads from verified state (Step 1) tells it which head is the
newest.

**What it proves:** every role whose evidence is in the transaction, the receipt, the
header or the body: `FROM`, `TO`, `EMITTER`, `TOPIC`, `WORD`, `AUTH`, `BLOCK`. The list is then complete
for those roles, and no longer rests on the importer: the importer key only says where the
slices are.

**What it does not prove:**
- **`TRACE`:** internal calls exist only through execution, so proving them means proving
  EVM execution of every block (a zkEVM). That is possible today, but far more expensive
  per block. The pragmatic split: prove the other roles, and keep `TRACE` on the
  importer's word, as now, marked as such per entry.
- **Availability:** a proof says the slice is right, not that Swarm still serves it. That
  stays the postage batch's job.

**What proving costs (order of magnitude, to be measured):**
- **The work per block:** RLP decoding of the transactions and receipts, Keccak over both
  tries (a few hundred KB per block at the tip), signature recovery for every sender and
  authority, the detection rules, and the encoding. No EVM execution. Current zkVMs prove whole mainnet blocks, execution included, in seconds
  on GPU clusters, so this is a much smaller job per block.
- **Forward from now:** about 7,200 blocks a day, plus the merges (recursion only, cheap
  next to the unit proofs).
- **Back over the 26 M blocks of history:** a one-off batch job. A guess of tens to
  hundreds of thousands of dollars at today's proving prices; measure before planning on
  it.
- **The prover:** any party with the blocks and receipts, for example a full node such as
  the nuc's Erigon. It needs no trust from the reader, so it need not be the importer.

## What changes for readers

- **Myotis's address history:**
  - **Step 1:** the history rests on one public view per importer key, with the range
    stated, as now.
  - **Step 3:** it is complete for every role but `TRACE`, without trusting the importer.

  The Swarm carve-out (`.claude/agent/data-sources.md` in Myotis) would need its "what it
  is trusted for" part rewritten, and the history could then say "proven complete" per
  range. That rule change is the owner's.
- **`eth_getLogs` from the index:** barred today by the same carve-out, because its
  completeness would rest on the importer in a method with no field to say so. Step 3,
  or EIP-7745, removes the reason, for the address-keyed part (`EMITTER`, and `TOPIC` for
  address-shaped topics). Arbitrary topics need a second index of the same shape, keyed by
  topic. Its size is of the same order as the address index (about 1,000–2,000 entries
  per block today).
- **Seeded log histories (RAILGUN):** these are a different source, so the index's proofs
  don't cover them directly. Two cheaper checks close most of the gap without any proof:
  - **The current root:** compare the rebuilt tree with the contract's current root, not
    with any root of `rootHistory`. That proves the commitments complete up to that block
    and rules out truncation-based partitioning. RAILGUN starts a new tree when one fills,
    and the current root is the active tree's. Each earlier tree needs its own check: that
    it is full, and that its final root is in that tree's `rootHistory`.
  - **Nullifiers in state:** read each own note's nullifier from the contract's state
    (assuming its nullifier mapping has a public getter), instead of trusting the
    nullifier logs.

  Both are verified state reads in Myotis. What then remains is a withheld note ciphertext,
  whose commitment the root check at least proves exists.

## A sensible order

1. **One view (Step 1)** for the index: a commitment per day, the bucket-hash file, and
   the check in readers. Small, and it ends partitioning.
2. **Fraud proofs (Step 2)** that readers can produce: Myotis's contradicted-entry state,
   turned into a portable proof. A bond only if there is more than one importer to choose
   from.
3. **A zk prover (Step 3)** for unit slices, forward from now, with recursion along the
   slice tree. Measure the cost per block first. History afterwards, if the cost allows.
4. **Watch EIP-7745.** If it is scheduled, `EMITTER` and `TOPIC` come from consensus, and
   Step 3 still covers `FROM`, `TO`, `WORD`, `AUTH` and `BLOCK`.

## Open questions

- **The cost per block** of the Step 3 statement in a current zkVM (SP1, RISC Zero, …), at
  the tip and in older eras. Proving time, cost and proof size.
- **The statement's size:** is one unit slice (64 blocks) the right proving unit, or one
  block, aggregated?
- **Encoding F in a zkVM:** the Rust encoder (`src/addr/`) compiled for the zkVM, or a
  circuit of its own? Compiling the same code keeps one definition of F.
- **The bucket path's layout:** the Merkle tree's levels as a chunked Swarm file, so that
  one path costs a few chunks even at the top levels (millions of buckets). It adds about
  `32·B` bytes per slice, from a few KB at level 0 to hundreds of MB at the top.
- **Where commitments go:** which chain (Ethereum mainnet for its block hashes, or Gnosis
  Chain where the postage is), and per what period.
- **EIP-7745's status and design,** read again before Step 3 is planned.
