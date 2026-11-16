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
- Address → transaction index (design, not implemented): a mobile-friendly index on Swarm
  from an address to the transactions it appears in — see
  [docs/address-index.md](docs/address-index.md).
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
