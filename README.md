# eth2swarm

Importer of Ethereum blocks into Ethereum Swarm.

- `ethswarm` — Rust CLI (binary). Talks to an execution-layer JSON-RPC endpoint to read
  block data, set with `--rpc` or `$ETH2SWARM_RPC` (e.g. `http://localhost:8546` for the
  patched bee-factory Anvil); which node to use is under
  [Ethereum mainnet nodes](#ethereum-mainnet-nodes).

- Blocks are stored as Single Owner Chunks with a header accumulator, so a client can
  verify any block from a single trust anchor — see
  [docs/soc-block-store.md](docs/soc-block-store.md) (`import`, `get`, `verify-epoch`,
  `header-stats`).
- Checkpoint table for Myotis: `checkpoint-table`, the hash of every 1024th block from genesis
  up to a finalized block, every header hashed and linked here, the top pinned by
  `--top-hash` or by another node (`--cross-check`); `--extend <table>` continues an existing
  table from its last entry, which it must reproduce, up to a new top.
- Address → transaction index (#8, on `main` since 2026-11-21): a mobile-friendly index on
  Swarm from an address to the transactions it appears in — design, implementation status
  and deployments in
  [docs/address-index.md](docs/address-index.md), every command in order in
  [docs/runbook.md](docs/runbook.md); `appearances`, `addr-stats`, `addr-import`,
  `addr-upload`, `addr-verify`, `addr-lookup`, `stamps` and `stamps plan` for postage batches
  (and `devnet-activity` for test traffic).
- State on Swarm (design sketch, not implemented): accounts, storage and code on Swarm,
  read by a light client and checked against the state root, with no trust in the
  publisher — see [docs/state-on-swarm.md](docs/state-on-swarm.md).
- Completeness proofs for the address index (design sketch, not implemented): a public
  commitment for one view, fraud proofs, and zk proofs that a slice is complete — see
  [docs/completeness-proofs.md](docs/completeness-proofs.md).
- `ethswarm serve` — local web UI to inspect SOCs, blocks, epochs and references on the
  Swarm nodes; see [docs/inspector.md](docs/inspector.md).

## Ethereum mainnet nodes

- **The nuc's Erigon is the main mainnet node:** the v3.8.1 archive node at
  `http://127.0.0.1:8645` on the nuc, listening on loopback only. On the nuc, pass it
  explicitly (`--rpc http://127.0.0.1:8645`, or `ETH2SWARM_RPC`). It is the source for
  imports, traces (`trace_block` and the frame index, neither of which zbox's geth has),
  measurements and the checkpoint table. Its load rules are in [docs/runbook.md](docs/runbook.md), step 0.
- **zbox's geth is the fallback and the second opinion:** `http://zbox.tail2ab6c8.ts.net:8561`,
  over Tailscale, full history without archive state and without traces. It is the
  independent cross-check (`checkpoint-table --cross-check`), the source of receipts
  before Byzantium (`--receipts-rpc`: Erigon 3 drops their state root), and the node to
  use where the nuc's is not reachable or is behind. It is also `ethswarm`'s built-in
  `--rpc` default, being the one node every machine reaches.

## Development environment

The local Swarm dev stack (5 Bee nodes + Anvil + Swarm contracts) is managed by
**bee-factory**. Full setup instructions — identical on the nuc and the MacBook — are in
[docs/bee-factory-setup.md](docs/bee-factory-setup.md).

```sh
bee-factory start                              # bring up the stack
ETH2SWARM_RPC=http://localhost:8546 cargo run -- tip   # verify block access
bee-factory stop                               # tear down
```

## CI

| Workflow | When | What |
|---|---|---|
| `CI` (`.github/workflows/ci.yml`) | every PR, pushes to `main` | `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked` (fails if no test ran) |
| `integration` (`.github/workflows/integration.yml`) | PRs and pushes touching the code, toolchain, e2e script or workflow; weekly | starts bee-factory on the runner and runs `scripts/e2e-bee-factory.sh` against its Anvil chain: import, verified reads through another node, rejected bad inputs, and an inspector check |
| `claude-review` | PR opened / reopened / ready | automated review (needs the `CLAUDE_CODE_OAUTH_TOKEN` secret) |

The rustc version is pinned in `rust-toolchain.toml`, every action by commit, and
bee-factory by `ci/bee-factory/package-lock.json`; `.github/dependabot.yml` proposes
updates to them as PRs (7-day cooldown).

## License

eth2swarm is licensed under the [Apache License, Version 2.0](LICENSE).

Copyright 2026 Dirk Jäckel.
