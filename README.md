# eth2swarm

Importer of Ethereum blocks into Ethereum Swarm.

- `ethswarm` — Rust CLI (binary). Talks to an execution-layer JSON-RPC endpoint
  (`$ETH2SWARM_RPC`, default `http://localhost:8545`) to read block data.

## Development environment

The local Swarm dev stack (5 Bee nodes + Anvil + Swarm contracts) is managed by
**bee-factory**. Full setup instructions — identical on the nuc and the MacBook — are in
[docs/bee-factory-setup.md](docs/bee-factory-setup.md).

```sh
bee-factory start                              # bring up the stack
ETH2SWARM_RPC=http://localhost:8545 cargo run -- tip   # verify block access
bee-factory stop                               # tear down
```
