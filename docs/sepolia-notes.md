# Global Swarm option — Sepolia testnet notes

Research snapshot: 2026-11-15, verified against GitHub source (tag v2.8.3),
docs.ethswarm.org and release notes. This is the **later / optional** path; day-to-day dev
uses bee-factory (see [bee-factory-setup.md](bee-factory-setup.md)).

## Network model (Bee v2.8.x — latest is v2.8.3, Oct 2026; network minimum v2.8.0)

There is **no `--network <name>` flag** and no `sepolia`/`gnosis` network string. Two built-in
networks, selected by a boolean:

| Network  | Select with        | network-id | Incentives chain       | Token        | Bootnode                  |
|----------|--------------------|------------|------------------------|--------------|---------------------------|
| mainnet  | `--mainnet` (default, =true) | 1  | Gnosis Chain (chainID 100) | xBZZ (0xdBF3…68da), gas xDAI | built-in          |
| testnet  | `--mainnet=false`  | 10 (auto)  | Sepolia (chainID 11155111) | sBZZ (0x543d…40db), gas Sepolia ETH | `/dnsaddr/testnet.ethswarm.org` |

The Ethereum-mainnet→Gnosis migration of mainnet incentives happened back in the Bee
1.10–1.14 era (2022/23). What happened at Bee 2.0.0 (Mar 2024): the Swarm **testnet** moved
Goerli → Sepolia. Sepolia testnet is definitely still alive in late 2026 (2.8.1 removed
`bee dev` and points users at Sepolia; 2.8.3 fixed Sepolia gas-repricing breakage).

## Correction: Bee needs an execution RPC for block data

Bee full/light nodes **require** `--blockchain-rpc-endpoint` (default expects
`ws://localhost:8546`) and sync postage-stamp contract events from that chain from the
contract start block onward (postage start block 6 596 277 on Sepolia). Only *ultra-light*
nodes skip the RPC. (bee-factory satisfies this with Anvil, so our dev stack is unaffected.)

For Sepolia: point it at a Sepolia RPC (docs example `wss://sepolia.infura.io/ws/v3/<KEY>`;
free public Sepolia RPCs exist but docs warn they are unreliable for contract-event syncing).
On this nuc the options would be: our own geth+Nimbus/Lighthouse Sepolia pair (geth v1.17.x +
Lighthouse 8.x or Nimbus CL; CL does checkpoint sync via `--checkpoint-sync-url`, geth snaps
syncs; budget ≥1 TB NVMe to be safe, ~16 GB RAM), or a paid RPC provider.

## Funding & staking (Sepolia)

- Sepolia ETH: public faucets (Infura faucet, faucetlink.to/sepolia).
- sBZZ: **no official faucet** found — docs say Uniswap (testnet mode), token
  `0x543dDb01Ba47acB11de34891cD86B675F04840db`. This is the realistic bottleneck.
- Staking is only mandatory to earn redistribution rewards, not to run the node.
  Current minimum is **10 BZZ (non-refundable)** — the 24 figure is obsolete
  (20 with reserve doubling). Exact Sepolia-contract minimum unverified; check
  `GET /stake` → `minimumDeposit` (dynamic since 2.8.3) on a running node.

## DAppNode Swarm package (if we go this route on the nuc)

Package is now maintained by w3rkspacelabs (`w3rkspacelabs/DAppNodePackage-Swarm`); current
release ships **Bee 2.8.2** (installed here is 2.6.0 — outdated). There is no `BEE_NETWORK`
env var. To run the Sepolia network: set `BEE_MAINNET="false"` (network-id and bootnode then
default correctly) and replace `BEE_BLOCKCHAIN_RPC_ENDPOINT` (default is the Gnosis
`nethermind-xdai.dappnode:8545`) with a Sepolia endpoint — via the package env/`EXTRA_OPTS`
editor. A `sepolia-geth` DAppNode package can supply the EL; there is no Gnosis-style bundled
Sepolia CL in that package, so a CL (Lighthouse/Nimbus Sepolia package) is still needed for a
self-hosted endpoint.

## Open questions to re-verify when we actually deploy

1. Live Sepolia staking minimum (`GET /stake`).
2. Whether any community sBZZ faucet exists by then (docs only mention Uniswap).
3. Actual Sepolia disk usage on current client versions (only estimates here).

Sources: [releases](https://github.com/ethersphere/bee/releases) ·
[configuration](https://docs.ethswarm.org/docs/bee/working-with-bee/configuration) ·
[fund-your-node](https://docs.ethswarm.org/docs/bee/installation/fund-your-node) ·
[v2.8.3 announcement](https://www.ethswarm.org/blog/2026/bee-2-8-3-release/) ·
[DAppNode package](https://github.com/w3rkspacelabs/DAppNodePackage-Swarm)
