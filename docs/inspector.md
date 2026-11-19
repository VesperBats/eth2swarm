# Swarm inspector (`ethswarm serve`)

A local web UI for looking at what the Swarm nodes hold: SOCs, blocks, epochs and raw
`/bytes` references, decoded as eth2swarm records and Ethereum blocks. It uses the same
decoding and verification code as `ethswarm get` / `verify-epoch`, so a green check in the
UI means the CLI client would accept the data too.

```sh
export ETH2SWARM_OWNER=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266    # importer key (dev)
export ETH2SWARM_ANCHOR=0x…                                           # optional pinned anchor
cargo run -- serve                                                    # http://127.0.0.1:8090/
```

By default it offers these nodes (pick one in the header bar):

| Name | URL |
|---|---|
| antd | http://localhost:1733 (ant light node, see below) |
| queen | http://localhost:1633 |
| worker1 … worker4 | http://localhost:1635 / 1637 / 1639 / 1641 |

Override with repeated `--node name=url`.

Block and Epoch lookups, and SOC lookups by owner + identifier, need the SOC owner. Without
`--owner` / `ETH2SWARM_OWNER` the owner field starts empty and is outlined in amber; such a
lookup then fails with "owner address is required" until an address is entered there.

Settings (node, owner, chain ID, anchor) are kept in the browser's local storage; the URL
fragment (`#block/97`, `#bytes/<ref>`, …) makes views bookmarkable.

## Tabs

- **Block** — fetch the header SOC for block *n*, then run every check step by step (trust
  anchor source, SOC signature, header number, geometry, proof lengths, accumulator proof →
  anchor, body fetch, body vs. header roots). Shows the SOC location, the decoded record and proofs, the header fields and
  the body (transactions with recovered senders, ommers, withdrawals). ◀ ▶ step through blocks.
- **Epoch** — fetch and verify an epoch's header file; lists all headers (click to open).
- **SOC** — look up any SOC by *owner + identifier string*, *owner + identifier hex* or *SOC
  address*. Shows the address derivation, the raw SOC fields, the recovered signer and whether
  it matches, and decodes the payload if it is an eth2swarm record. Templates fill in
  `eth:<chain>:<anchor>:hdr:<n>`, `…:epoch:<k>` and `eth:<chain>:accumulator`.
- **Bytes** — fetch a `/bytes` reference and decode it as a block body or a header sequence
  (epoch file), with a hex preview.

Leaving the anchor empty uses the owner's published anchor. The verdict is then shown as an
amber warning rather than a green check, because it means trusting the publisher. Records
shown in the SOC and Epoch tabs carry a badge saying whether their signature was verified
against the configured owner.

Settings saved in the browser are dropped when `ethswarm serve` is restarted with different
defaults (e.g. a new `ETH2SWARM_ANCHOR` after a re-import); **reset settings** restores them.

## Reading through ant

To check what a light node sees, run `antd` on the bee-factory network (ID 1337) with the
running bee nodes as bootnodes, API on port 1733:

```sh
antd --network-id 1337 --api-addr 127.0.0.1:1733 --allow-private-dials --no-disk-cache \
  --gnosis-logs-rpc-url "" \
  --bootnodes /ip4/127.0.0.1/tcp/1634/p2p/<queen peer id>,…
```

Peer IDs: `curl -s localhost:1633/addresses` (use the `/ip4/127.0.0.1/…` underlay).

The UI has no authentication. It listens on `127.0.0.1:8090` by default and warns when
`--listen` is not a loopback address; requests whose `Host` header isn't the listen address
or a loopback name are rejected (protection against DNS rebinding).
