# SOC block store (prototype)

Ethereum blocks are stored on Swarm so that a client can look them up **by block number**
and verify them while trusting only two values: the **owner address** of the importer and a
32-byte **trust anchor**.

## Layout

| What | Swarm object | Identifier / reference |
|---|---|---|
| Header + proofs + body ref | SOC (≈ 0.9 KB at 64×4, ≈ 1.5 KB for mainnet geometry) | `keccak256("eth:<chain>:<anchor>:hdr:<n>")` |
| Block body (txs, ommers, withdrawals) | file via `/bytes`, RLP | referenced from the header SOC |
| Epoch: root + header file + master proof | SOC | `keccak256("eth:<chain>:<anchor>:epoch:<k>")` |
| Epoch header file (concatenated header RLPs) | file via `/bytes` | referenced from the epoch SOC |
| Published anchor (convenience only) | SOC | `keccak256("eth:<chain>:accumulator")` |

SOC address = `keccak256(identifier ‖ owner)`, so the reader needs the owner address; the
SOC signature (EIP-191 over `keccak256(identifier ‖ cac_address)`) is checked client-side.
`<anchor>` is the lower-case `0x` hex trust anchor: each accumulator gets its own immutable
set of SOCs, so a re-import with another range never overwrites (or races with) an earlier
one. Only the `accumulator` pointer is mutable.
Record formats are in [`src/layout.rs`](../src/layout.rs).

## Accumulator

Blocks are grouped into epochs of `epoch_size` (power of two) starting at block 0. Each
epoch's block hashes form a keccak256 Merkle tree; the epoch roots form a master tree:

```
anchor = keccak256(master_root ‖ num_epochs (u64 BE) ‖ epoch_size (u64 BE))
```

A header is verified with `log2(epoch_size) + ceil(log2(num_epochs))` sibling hashes — no
other headers are downloaded. The body is verified against the header's
`transactionsRoot`, `ommersHash` and `withdrawalsRoot`. This is the same idea as the Portal
Network pre-merge accumulator (SSZ/sha256 there). Post-merge mainnet would anchor to the
beacon chain's `historical_summaries` instead; ZK proofs are not needed for this.

Proof size for full mainnet with 8192-block epochs: 13 + 12 = 25 hashes = 800 bytes, so a
header SOC stays ≈ 1.5 KB.

## Measured header sizes (canonical RLP)

`ethswarm header-stats --step 250000` against mainnet (zbox), and Anvil (bee-factory):

| Fields | Fork shape | Range sampled | min / avg / max bytes | per 4 KB chunk |
|---|---|---|---|---|
| 15 | Frontier → Berlin | #0 – #12.75M | 519 / 532 / 541 | 7 |
| 16 | London (baseFee) | #13M – #17M | 516 / 538 / 548 | 7 |
| 17 | Shanghai (withdrawalsRoot) | #17.25M – #19.25M | 550 / 567 / 581 | 7 |
| 20 | Cancun (blob fields, beacon root) | #19.5M – #22.25M | 603 / 610 / 616 | 6 |
| 21 | Prague (requestsHash) | #22.5M – #26.25M | 632 / 645 / 650 | 6 |
| 21 | Anvil (Prague) | #0 – #1000 | 606 / 608 / 613 | 6 |

Headers are variable-length (RLP minimal integers, variable `extraData`); RLP carries no
field names.

## Try it on bee-factory

```sh
bee-factory start --tag v2.8.3
scripts/e2e-bee-factory.sh            # mainnet blocks (default: zbox) → local Swarm, verify via queen
ETH2SWARM_RPC=http://localhost:8545 scripts/e2e-bee-factory.sh   # same against local Anvil
```

Blocks are read from Ethereum mainnet: on the nuc from its Erigon
(`ETH2SWARM_RPC=http://127.0.0.1:8645`), elsewhere from zbox, the built-in default `--rpc`
(README, *Ethereum mainnet nodes*); only the storage side is the local bee-factory Swarm. Verified on 2026-11-16: mainnet blocks 0–255 (genesis
`0xd4e5…8fa3`, 59 blocks with uncles) imported and verified through the queen.

Manual use:

```sh
# import mainnet blocks (on the nuc add --rpc http://127.0.0.1:8645; the default is zbox) (via worker 1, which owns the postage batch); prints the trust anchor
cargo run -- --bee http://localhost:1635 import --key <hex key> --epoch-size 64 --epochs 4
# verify (via the queen → proves p2p retrieval)
cargo run -- --bee http://localhost:1633 get 3 --owner <owner> --anchor <anchor>
cargo run -- --bee http://localhost:1633 verify-epoch 0 --owner <owner> --anchor <anchor>
```

The client's `--chain-id` defaults to 1 (mainnet); pass `--chain-id 1337` for data
imported from the bee-factory Anvil.

## Limitations of the prototype

- Imports from block 0 and holds the range in memory; uploads sequentially.
- Receipts are not stored yet.
- No incremental accumulator: extending the range changes the anchor (as with Portal's
  frozen pre-merge accumulator, the intended use is fixed, finalized ranges).
- Anyone who knows the owner key controls availability; integrity only depends on the anchor.
- Integrity requires a **pinned** `--anchor`. Without it the client falls back to the owner's
  published anchor, i.e. it trusts the owner completely.
- Client downloads are capped (16 MiB per body, 1 KiB per header in an epoch file).
