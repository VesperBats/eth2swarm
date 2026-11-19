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

Override with repeated `--node name=url`. Settings (node, owner, chain ID, anchor) are kept in
the browser's local storage; the URL fragment (`#block/97`, `#bytes/<ref>`, …) makes views
bookmarkable.

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
- **Address index** — the addr1 index ([address-index.md](address-index.md)) of the SOC
  owner on the chain ID in the settings bar, read with the code `ethswarm addr-lookup` uses
  (every SOC signature, chunk and file reference checked):
  - *Look up* an address: the head (the newest found by probing head updates, or the one with
    the given units), what the index covers (an index from a later block says where it
    starts), the slices read, and the address's entries newest first (block, transaction or
    block slot, roles). *Since unit* reads incrementally.
  - Links open a **slice** (its descriptor, and the bucket the address falls into), a
    **bucket** (its records by fingerprint, the address's highlighted, inline entries and
    spill files; a paged bucket with its pages file's records) and a **spill file** (its
    entries). Views are bookmarkable (`#addr/lookup:<address>`, `#addr/slice:<L>:<i>`,
    `#addr/bucket:<L>:<i>:<k>`, `#addr/spill:<L>:<i>:<ref>:<count>`).
  - The SOC tab decodes addr1 heads, slice descriptors and buckets too, with templates for
    `eth:<chain>:addr1:origin` and `eth:<chain>:addr1:slice:<L>:<i>`.

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
