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
    the given units; the inspector remembers the newest it found per node, owner and chain and
    probes on from there, so a repeated lookup costs one probe), what the index covers (an index from a later block says where it
    starts), the slices read, and the address's entries newest first (block, transaction or
    block slot, roles). *Since unit* reads incrementally.
  - Links open a **slice** (its descriptor, and the bucket the address falls into), a
    **bucket** (its records by fingerprint, the address's highlighted, inline entries and
    spill files; a paged bucket with its pages file's records) and a **spill file** (its
    entries). Views are bookmarkable: `#addr/lookup:<address>:<units>:<since>`,
    `#addr/index:<units>:<since>` (the head without an address), `#addr/slice:<L>:<i>:<address>`,
    `#addr/bucket:<L>:<i>:<k>:<address>` (the address is optional and is what the view
    highlights), `#addr/spill:<L>:<i>:<ref>:<count>` and `#addr/examples:<units>`. A lookup and a spill file show their
    newest 500 entries (the API's `limit`), newest first, and always the full count. A spill file
    above 256 MB is read from its last page backwards and only as far as the limit: the view
    says so (`read`: newest pages only), and its count is the bucket record's, not checked
    against the file.
  - A failed read keeps what was read before it: a lookup that fails in one slice still shows
    the head and the slice links, a bucket or spill view still shows the slice.
  - *Show example addresses*: well-known addresses of the chain (mainnet: tokens, the
    deposit contract, a router, block builders; dev chains: the Anvil accounts), each with
    the number of entries it has in the index, counted from the buckets alone (no spill
    files), read in parallel. An address whose bucket cannot be read shows why, and the others
    are still counted. A click looks the address up as of the head the count was made at. The
    index stores fingerprints, not addresses, so it cannot list its own.
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

## Opening it from another machine

The UI has no authentication. It listens on `127.0.0.1:8090` by default and warns when it
listens anywhere else. To open it from another machine:

```sh
cargo run -- serve --tailscale        # this machine's Tailscale IPv4, port 8090
cargo run -- serve --listen 0.0.0.0:8090   # every address (LAN included)
```

`--tailscale` listens on the Tailscale address only, not on the LAN or loopback addresses.
That alone does not make it tailnet-only: Linux accepts a packet for any of its addresses on
any interface, so a host on the same LAN can add a route to the Tailscale address via this
machine and reach the port. Tailscale's ACLs do not see that path; a host firewall that drops
traffic to the port from anything but `tailscale0` closes it.

`--tailscale` also accepts the machine's full MagicDNS name
(`http://<host>.<tailnet>.ts.net:8090/`); when `tailscale status` gives none, the inspector
says so at startup and accepts only the IP. The short host name, which most tailnets resolve
too, is not accepted unless given with `--allow-host <host>`.

Requests are answered only for a `Host` header that is, on the listen port, the listen
address, a loopback address or `localhost` (any IP address when listening on `0.0.0.0`), or
one given with `--allow-host` on its own port: `name`, `name:port`, `ip` or `ip:port`, the
port defaulting to the listen port. A port forward or proxy on another port needs its entry,
e.g. `--allow-host localhost:9000` for `ssh -L 9000:127.0.0.1:8090`, or `--allow-host
inspector.example:443` for a TLS proxy that passes the `Host` on (a `Host` without a port
matches an entry on 80 or 443). Other names are rejected, which keeps a web page from reaching
the inspector through a DNS name it controls (DNS rebinding).
