# eth2swarm

Importer of Ethereum blocks into Ethereum Swarm.

- `ethswarm` — Rust CLI (binary). Talks to an execution-layer JSON-RPC endpoint to read
  block data. Default: `http://zbox.tail2ab6c8.ts.net:8561` (zbox mainnet node, reachable
  over Tailscale); override with `$ETH2SWARM_RPC` or `--rpc`
  (e.g. `http://localhost:8546` for the patched bee-factory Anvil).

- Blocks are stored as Single Owner Chunks with a header accumulator, so a client can
  verify any block from a single trust anchor — see
  [docs/soc-block-store.md](docs/soc-block-store.md) (`import`, `get`, `verify-epoch`,
  `header-stats`).
- Address → transaction index (in progress, #8): a mobile-friendly index on Swarm from an
  address to the transactions it appears in — design in
  [docs/address-index.md](docs/address-index.md); `appearances`, `addr-stats`, `addr-import`,
  `addr-upload`, `addr-verify`, `addr-lookup` (and `devnet-activity` for test traffic).
- State on Swarm (design sketch, not implemented): accounts, storage and code on Swarm,
  read by a light client and checked against the state root, with no trust in the
  publisher — see [docs/state-on-swarm.md](docs/state-on-swarm.md).
- `ethswarm serve` — local web UI to inspect SOCs, blocks, epochs and references on the
  Swarm nodes; see [docs/inspector.md](docs/inspector.md).

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
