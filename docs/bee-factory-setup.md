# Swarm dev environment with bee-factory

This project's development stack is [bee-factory](https://github.com/ethersphere/bee-factory)
(npm: `@ethersphere/bee-factory`): it spins up **5 Bee nodes + an Anvil dev chain** with all
Swarm contracts deployed and wired up (stake, postage batch, a SWAP cheque) in one command.

Because the blockchain is Anvil (a standalone EVM dev node), **no EL/CL pair is needed** —
Anvil serves the JSON-RPC that Bee nodes and `ethswarm` talk to. The trade-off: it is a private
chain (ID 1337), so the stack does **not** participate in the global Swarm network.
See “Alternative: global Swarm” at the bottom.

## Prerequisites

| Requirement | Notes |
|---|---|
| Docker | must be usable without `sudo` (Linux: user in `docker` group) |
| Node.js ≥ 18 | verified working: v26; the npm CLI runs the stack |
| Free host ports | `8545` (Anvil) and `1633`–`1642` (Bee API/P2P) |

### macOS (MacBook)

```sh
brew install node docker        # Docker Desktop app works too
```

### Linux (nuc / DAppNode)

Node 26 is already installed under `/usr/local`; `bee-factory` was installed with a
user-level npm prefix (system-wide `/usr/local` requires root):

```sh
npm config set prefix ~/.npm-global
# make sure ~/.npm-global/bin is on your PATH (add to ~/.zshrc)
export PATH="$HOME/.npm-global/bin:$PATH"
```

## Install

```sh
npm install -g --allow-scripts=protobufjs,ssh2 @ethersphere/bee-factory
bee-factory --help
```

(`--allow-scripts` is needed on npm ≥ 11 because dependencies run postinstall scripts.)

## Port conflicts (nuc only)

On the nuc two *other* containers occupy ports bee-factory needs:

- `nethermind` (Gnosis chain EL) → **8545**
- `DAppNodePackage-bee.swarm.public.dappnode.eth` → **1634**

Stop them before `bee-factory start` (and be aware this pauses Gnosis staking until restored):

```sh
docker stop nethermind DAppNodePackage-bee.swarm.public.dappnode.eth
```

The two setups are **mutually exclusive** while running (shared ports): after `bee-factory stop`,
restore with

```sh
docker start nethermind DAppNodePackage-bee.swarm.public.dappnode.eth
```

Check for whatever else sits on those ports (macOS/Linux):

```sh
# macOS
lsof -iTCP -sTCP:LISTEN -P | grep -E ':(8545|163[3-9]|164[0-2])\b'
# Linux
ss -ltnp | grep -E ':(8545|163[3-9]|164[0-2])\b'
```

## Start

```sh
bee-factory start               # fast boot from bundled snapshot
bee-factory start --fresh       # redeploy contracts from scratch, re-save snapshot
bee-factory start --tag v2.8.3  # build Bee from a specific ref (default: master)
```

First run: clones `ethersphere/bee`, builds `bee-factory/bee:<ref>` via `Dockerfile.dev`
(Go build, several minutes) and pulls `ghcr.io/foundry-rs/foundry:latest`. The build is cached;
subsequent starts are fast. **Tip:** pin `--tag` to a release (e.g. `v2.8.3`) instead of
tracking `master`.

The command returns once all nodes report `isWarmingUp: false`.

## Endpoints

| Node     | API                   | P2P                 |
|----------|-----------------------|---------------------|
| Queen    | http://localhost:1633 | localhost:1634      |
| Worker 1 | http://localhost:1635 | localhost:1636      |
| Worker 2 | http://localhost:1637 | localhost:1638      |
| Worker 3 | http://localhost:1639 | localhost:1640      |
| Worker 4 | http://localhost:1641 | localhost:1642      |

**Anvil RPC:** `http://localhost:8545` (chain ID 1337, 1 s block time)

Smoke tests (verified against Bee 2.8.3 — note the endpoints are `/status` and `/topology`,
not `/node/status`):

```sh
curl -s localhost:1633/status | python3 -m json.tool      # expect beeMode=full, connectedPeers=4, isWarmingUp=false
curl -s localhost:1633/topology | python3 -m json.tool    # expect population=4
ETH2SWARM_RPC=http://localhost:8545 cargo run -- tip      # expect Chain ID: 1337
ETH2SWARM_RPC=http://localhost:8545 cargo run -- block 0
```

Roundtrip upload/download (the postage batch is bought on **worker 1**, not the queen):

```sh
BATCH=$(curl -s localhost:1635/stamps | python3 -c "import json,sys; print(json.load(sys.stdin)['stamps'][0]['batchID'])")
REF=$(curl -s -X POST -H "Swarm-Postage-Batch-Id: $BATCH" --data-binary "hello swarm" localhost:1635/bytes | python3 -c "import json,sys; print(json.load(sys.stdin)['reference'])")
curl -s localhost:1633/bytes/$REF                          # fetch via queen → proves p2p sync
```

## Contracts & wallets (Foundry test keys — never use in production)

Deployed by the stack: BzzToken, PostageStamp, PriceOracle, StakeRegistry,
Redistribution, SimpleSwapFactory, SwapPriceOracle (addresses printed at startup).

Node wallets (each funded with 1 ETH + 100 BZZ, node password `bee-factory`):

| Node            | Address | Private key |
|---|---|---|
| queen | `0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc` | `0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba` |
| worker 1 | `0xc512CF05d75c4B4818a04d9c65B974168828764A` | `0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec15649` |
| worker 2 | `0x14dC79964da2C08b23698B3D3cc7Ca32193d9955` | `0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356` |
| worker 3 | `0x92A6095F965d30afFCC9da457d8fDB44ccB98F92` | `0xdbda1821b80551c9d65939329250132c444d73f6a52f3fbdb50866273e8f41f1` |
| worker 4 | `0xa0Ee7A142d267C1f36714E4a8F75612F20a79720` | `0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6` |

Pre-wired state worth knowing:

- Worker 1 → Worker 2 SWAP cheque is ready to cash at `:1637`.
- Withdrawal address whitelist: `0xd238ff944bacb478cbed5efcae784d7bf4f2ff80` (all other
  withdrawal targets are rejected with HTTP 400).
- Chain is advanced ~160 blocks after upload so reserve sampling / `rchash` succeed.

## Stop

```sh
bee-factory stop   # removes containers + docker network (bee-factory-net)
```

## ethswarm configuration

`ethswarm` defaults to the zbox mainnet node (`http://zbox.tail2ab6c8.ts.net:8561`).
Against the bee-factory stack point it at Anvil instead:

```sh
export ETH2SWARM_RPC=http://localhost:8545
cargo run -- tip
```

## Alternative: global Swarm (not used for day-to-day dev)

The stack is a private chain, so no global reach and no faucets. If we later want a Bee node
in a global network we need a real chain instead of Anvil — Bee selects between Swarm mainnet
(Gnosis Chain) and Swarm testnet (Sepolia) with `--mainnet=false`, and full nodes **require**
an execution RPC for that chain (`--blockchain-rpc-endpoint`). Details, funding/staking facts
and the DAppNode recipe: [docs/sepolia-notes.md](sepolia-notes.md).

## Known quirks

- Nethermind on **Gnosis chain** returns xDai-style block headers (no `mixHash`, extra
  `signature`/`step` fields) which strict Ethers/alloy RPC deserializers reject. ethswarm
  works fine against standard chains (Anvil, Sepolia, Ethereum mainnet).
