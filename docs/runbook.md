# Runbook: building, publishing and checking the address index

The exact commands, in order, from Erigon's archive node to an index on Swarm, plus the
checkpoint table Myotis bundles. Each step gives the **one-month** variant (an index from a
later block, for testing) and the **full** variant (from genesis). The design behind the
commands is in [address-index.md](address-index.md). The local Swarm is described in
[bee-factory-setup.md](bee-factory-setup.md). Every `ethswarm` flag here is in `ethswarm
<command> --help`, and those of `erigon-trace-export` in `erigon-trace-export -h`.

The paths and hosts are the nuc's:
- **Erigon** is the v3.8.1 archive node at `http://127.0.0.1:8645`, with its datadir at
  `~/erigon-data/mainnet-archive`.
- **geth** is zbox's, at `http://zbox.tail2ab6c8.ts.net:8561`, over Tailscale.
- **bee-factory** runs patched: API on 1733–1742, Anvil on 8546.

Replace them elsewhere.

## 0. Before anything

**Load rules on the nuc** (32 GB until the upgrade):
- **One heavy client on Erigon at a time.** Heavy means `trace_block`, the frame-index export,
  or an import fetching blocks. A header-only `checkpoint-table` run is light.
- **Everything long runs in a memory-limited systemd unit or scope:** `MemoryHigh=3G
  MemoryMax=6G` for imports and exports. The worst case then kills the job, not Erigon:
  systemd-oomd once killed Erigon under combined load.
- **`--concurrency 4` and `--sort-memory-mb 128`** until the RAM upgrade. With 64 GB, measure
  8 and 16 before raising.
- **Disk:** a full run peaks at about 1.3 TB (the export directory, sort runs, the frame
  index). Check `df -h ~` first.

**Postage:** `.claude/agent/postage.md` governs every upload to a real Bee node and every
batch purchase. Ask before using a batch; propose a new one, never buy it yourself; use
immutable batches only. bee-factory, which answers `GET /wallet` with `chainID` 1337, is
exempt from asking and proposing, but not from the last rule: immutable batches there too.

**Build the tools:**
```sh
# ethswarm (from the repository root)
systemd-run --user --scope -q -p MemoryHigh=4G -p MemoryMax=8G cargo build --release --locked -j 4
install -m 755 target/release/ethswarm ~/bin/ethswarm   # a new file, so a running unit keeps its own

# erigon-trace-export (Go 1.26+, gcc 10+; on the nuc /usr/local/go/bin/go and gcc-13)
cd tools/erigon-trace-export
CC=gcc-13 CXX=g++-13 /usr/local/go/bin/go build -o ~/bin/erigon-trace-export .
```

**A key for the index:** the SOC owner is the importer's key. One key is one index, and the
key must never have published another one.
```sh
mkdir -p ~/addr1-<name>/state
(umask 077; echo "0x$(openssl rand -hex 32)" > ~/addr1-<name>/key)
```
Commands take it as `--key "$(cat key)"`, or via `ETH2SWARM_KEY`, which keeps it off the
process list.

## 1. Export Erigon's frame index (`TRACE`)

Erigon keeps an index of every call frame's addresses (`tracesfrom`, `tracesto`).
`erigon-trace-export` writes it as A1TR files, which the import reads instead of
`trace_block`; that is about 350 blocks/s against `trace_block`'s 1–2. Skip this step for an
index without traces: then `TRACE` is not detected at all.

```sh
FINAL=$(curl -s -X POST -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["finalized",false]}' \
  http://127.0.0.1:8645 | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"]["number"],16))')

# One month (≈ 217,000 blocks: ~10 min, ~7 GB), into a directory of its own
systemd-run --user --scope -q -p MemoryHigh=3G -p MemoryMax=6G \
  ~/bin/erigon-trace-export --datadir ~/erigon-data/mainnet-archive \
  --from 26236928 --to "$FINAL" --out ~/erigon-data/a1tr/mainnet-month

# Full, from genesis (≈ 0.5–1 day in all, a few hundred GB), in ranges of 2,000,000 blocks:
# a run that stops loses its whole range (see below), so a range is the unit of retry
for FROM in $(seq 0 2000000 "$FINAL"); do
  TO=$(( FROM + 1999999 < FINAL ? FROM + 1999999 : FINAL ))
  systemd-run --user --scope -q -p MemoryHigh=3G -p MemoryMax=6G \
    ~/bin/erigon-trace-export --datadir ~/erigon-data/mainnet-archive \
    --from "$FROM" --to "$TO" --out ~/erigon-data/a1tr/mainnet >> ~/a1tr-export.log 2>&1 \
    || { echo "range $FROM..=$TO failed: rerun it"; break; }
done
```

- **Files:** one per 100,000 blocks (`--blocks-per-file`), named `trace-<first>-<last>.a1tr`,
  aligned to multiples of 100,000 and clipped to `--from`/`--to`.
- **A stopped run writes nothing.** The export first scans every index file of its range,
  which is most of the time, and writes its files only at the end. A rerun of a range
  replaces its files; it does not skip them.
- **One directory per index.** The month's files have other boundaries than a full export's
  (`trace-0026236928-0026299999` against `trace-0026200000-0026299999`), and an export refuses
  a directory holding a file that overlaps its range with other boundaries.
- **Finality:** `--to` may not be above the datadir's finalized block, unless
  `--allow-unfinalized` is given.
- **A sort buffer** of 128 MB by default (`--sort-buffer-mb`), in `<out>/.tmp`.
- **Check the files** against `trace_block` on a few ranges before importing. Edge cases are
  self-destructs, failed creations, and pre-Byzantium blocks (which need `--receipts-rpc`):
  ```sh
  ethswarm --rpc http://127.0.0.1:8645 trace-compare --trace-index ~/erigon-data/a1tr/mainnet-month \
    --from 26400000 --to 26400999
  ```
  An appearance only `trace_block` has is an error. One only the index has is listed: a
  failed internal creation, which the `TRACE` rule counts.

## 2. Build the index (`addr-import`)

The import fetches every block, with its receipts, from `--rpc`, and verifies them against
the header. It takes `TRACE` from the frame index and writes either straight to a Bee node or
to an **export directory** (`--out`) that `addr-upload` sends later. Use `--out` for anything
long: it needs no Bee node and no postage while it runs.

```sh
cd ~/addr1-<name>
# The target: every unit the frame index covers. LAST is the last block of the A1TR files
# (the export's --to); a unit is 64 blocks.
LAST=<the export's --to>
UNITS=$(( (LAST + 1) / 64 ))

# One month, from block 26,236,928 (a multiple of 64)
systemd-run --user --unit addr1-<name>-import -p MemoryHigh=3G -p MemoryMax=6G \
  --working-directory="$PWD" sh -c "ETH2SWARM_KEY=\$(cat key) exec ~/bin/ethswarm \
    --rpc http://127.0.0.1:8645 addr-import --state state --out export --init \
    --from-block 26236928 --units $UNITS --trace-index ~/erigon-data/a1tr/mainnet-month \
    --concurrency 4 --sort-memory-mb 128 >> import.log 2>&1"

# Full, from genesis: no --from-block. Receipts before Byzantium (block 4,370,000) come from
# geth, since Erigon 3 serves them without the state root their receiptsRoot covers.
systemd-run --user --unit addr1-full-import -p MemoryHigh=3G -p MemoryMax=6G \
  --working-directory="$PWD" sh -c "ETH2SWARM_KEY=\$(cat key) exec ~/bin/ethswarm \
    --rpc http://127.0.0.1:8645 --receipts-rpc http://zbox.tail2ab6c8.ts.net:8561 \
    addr-import --state state --out export --init --units $UNITS \
    --trace-index ~/erigon-data/a1tr/mainnet --concurrency 4 --sort-memory-mb 128 \
    >> import.log 2>&1"
```

`$UNITS` is expanded when the unit is started; `\$(cat key)` when it runs, so the key stays
off the command line systemd records.

- **`--init` only on the first run.** Later runs resume from `--state`, and pass the same
  `--out`.
- **`--units`:** with `--trace-index`, the files must cover every block from the index's
  start to `--units · 64 − 1`. The run refuses otherwise, naming the first uncovered block and
  the largest `--units` the files allow. Without `--units` the target is every finalized
  unit, which is usually past the export.
- **Timing:** about 35–40 blocks/s at concurrency 4 on today's nuc. A month takes about
  1 h 45 min. The full history takes about 4–8 days, likely less with 64 GB and a higher
  concurrency.
- **The backfill saves no progress per slice.** A run that stops, from a reboot or a kill,
  repeats the whole backfill when it resumes: "resuming the unfinished backfill as it
  started". Plan the full run so it is not interrupted.
- **Progress:** `tail -f import.log` shows one line per backfill slice, then one per head.
- **Without traces:** leave out `--trace-index` (and `--receipts-rpc` if the start is after
  Byzantium). `--traces` would read `trace_block` instead, which is exact for everything
  except a failed internal creation, but runs at 1–2 blocks/s.

## 3. Postage batches

```sh
# The node's batches: depth, fill of the fullest bucket, immutable or not, lifetime, which
# export uses which
ethswarm --bee http://localhost:1735 stamps --export ~/addr1-<name>/export

# What an export needs: the smallest depth with the fullest bucket at most half full, and with
# --days the cost and the command to buy it (immutable, labelled)
ethswarm --bee <bee> stamps plan ~/addr1-<name>/export --days 30 \
  --label "addr1 chain 1, owner <owner>, from <first block>"
```

- **The example:** the month index stamps 779,755 chunks with a fullest bucket of 32, which
  calls for depth 22.
- **Lower batches:** with lower slices in the export, `plan` also sizes the permanent batch
  and a lower batch apart. A lower batch needs only its slices' lifetime plus `--grace-days`
  (default 7). Run `plan` again with those `--days` and its own `--label`.
- **On bee-factory** blocks are 1 s, so pass `--block-secs 1`. On Swarm mainnet (Gnosis
  Chain) the default of 5 s is right.
- **Buying, topping up and diluting are the owner's.** `plan` prints the command and buys
  nothing. Bee refuses a balance of 24 hours or less, and `plan` refuses to print one.
- **Record every batch ID an index uses** in its PR or issue. `addr-upload` keeps only the
  permanent one, in `export/uploaded.json`.

## 4. Upload

```sh
# To bee-factory (worker 1 holds the funds and the batches)
ethswarm --bee http://localhost:1735 addr-upload ~/addr1-<name>/export --batch <batch id> \
  --concurrency 8

# With lower batches for the short-lived slices
ethswarm --bee <bee> addr-upload ~/addr1-<name>/export --batch <permanent batch> \
  --lower-batch <batch> --lower-batch <batch> --concurrency 8
```

- **Resumable:** an upload resumes where the last one of the directory stopped.
  `--from-start` sends everything again, which another batch requires.
- **Origin check:** the upload checks the index's origin first, and refuses an export whose
  key already published another index.
- **Mainnet Swarm** takes the same command, with `--bee` pointed at a Bee node on Swarm
  mainnet: Gnosis Chain, a funded wallet, and batches the owner bought. No such node is set up
  yet; see [sepolia-notes.md](sepolia-notes.md) for running one. A month is about 1.7 GB of
  export and half an hour of upload on bee-factory.

## 5. Keep it current

An index without `--trace-index` can follow the chain. One unit is 64 blocks, about 13
minutes, and is imported once it is finalized.

```sh
cd ~/addr1-<name>
systemd-run --user --unit addr1-<name>-follow -p MemoryHigh=3G -p MemoryMax=6G \
  --working-directory="$PWD" sh -c 'ETH2SWARM_KEY=$(cat key) exec ~/bin/ethswarm \
    --rpc http://127.0.0.1:8645 addr-import --state state --out export --follow \
    --concurrency 4 --sort-memory-mb 128 >> follow.log 2>&1'
systemd-run --user --unit addr1-<name>-upload -p MemoryHigh=1G -p MemoryMax=2G \
  --working-directory="$PWD" sh -c 'while true; do ~/bin/ethswarm --bee http://localhost:1735 \
    addr-upload export --batch <batch id> --concurrency 8 >> upload-follow.log 2>&1; \
    sleep 600; done'
```

`--follow` is refused with `--trace-index`: the files end where their export ended. A traced
index can be extended in steps instead:
1. Export the new blocks into the same directory, `--from <the previous --to + 1>`. A file
   boundary would leave a gap, and an earlier start overlaps a file with other boundaries.
2. Run `addr-import --units <new target>` with the same `--state`, `--out` and
   `--trace-index`. The files of blocks already imported must stay, since merges read them
   again.

## 6. Check

```sh
# The published index: every slice, bucket and spill file of the head (default: the origin)
ethswarm --bee http://localhost:1733 addr-verify --owner <owner> --chain-id 1 [--units <n>]

# An address, newest first; with --against-chain compared with the chain itself over the same
# blocks (TRACE from the frame index with --trace-index, else trace_block)
ethswarm --bee http://localhost:1733 --rpc http://127.0.0.1:8645 addr-lookup <address> \
  --owner <owner> --chain-id 1 --limit 20 \
  --against-chain --trace-index ~/erigon-data/a1tr/mainnet --concurrency 4

# The inspector: heads, slices, buckets and lookups in a browser
ethswarm --rpc http://127.0.0.1:8645 serve --owner <owner> [--tailscale]
```

Read from the queen (1733), not the node you uploaded through, so that a lookup also proves
the data reached the network.

## 7. Measure (`addr-stats`)

The tables in [address-index.md](address-index.md) (*Measurements*) come from:

```sh
# Per era: nine windows of 2,048 blocks (≈ 3–6 h with trace_block)
systemd-run --user --scope -q -p MemoryHigh=3G -p MemoryMax=6G \
  ~/bin/ethswarm addr-stats --rpc http://127.0.0.1:8645 \
  --receipts-rpc http://zbox.tail2ab6c8.ts.net:8561 --traces --concurrency 4 --level 5 \
  --at 1000000 --at 4000000 --at 7000000 --at 10000000 --at 13000000 \
  --at 16000000 --at 19000000 --at 22000000 --at 26400000 > level5.md 2> level5.log

# Slice lengths: 16,384 blocks
... addr-stats ... --level 8 --at 26400000 > level8.md 2> level8.log
```

Progress goes to the log every 30 s. A window before Byzantium needs `--receipts-rpc`.

## 8. The checkpoint table for Myotis

The hash of every 1024th block, from genesis up to a finalized block. Myotis bundles it as a
trust anchor (its `docs/ideas/deep-block-verification.md`, decision D1).
- **The walk:** every header is hashed here and linked by `parentHash` down to the genesis
  hash. None of the node's hashes is used.
- **The top:** pinned by a second source, `--top-hash` or `--cross-check`, before anything is
  written. A run with neither is refused.

```sh
mkdir -p ~/checkpoint-table && cd ~/checkpoint-table

# Full build (≈ 40 min; header-only, light on Erigon; resumes from <out>.work after a stop)
systemd-run --user --unit checkpoint-table -p MemoryHigh=2G -p MemoryMax=4G \
  --working-directory="$PWD" sh -c 'exec ~/bin/ethswarm --rpc http://127.0.0.1:8645 \
    checkpoint-table --out mainnet-1024.hashes --concurrency 8 \
    --cross-check http://zbox.tail2ab6c8.ts.net:8561 > table.out 2> table.log'

# Extend for a release (seconds to minutes): from the last entry up to the new finalized block
ethswarm --rpc http://127.0.0.1:8645 checkpoint-table --extend mainnet-1024.hashes \
  --out mainnet-1024.next.hashes --cross-check http://zbox.tail2ab6c8.ts.net:8561
```

**Output:** `<out>` is 32 bytes per entry, entry *i* the hash of block *i*·1024. `<out>.json`
is its sidecar: the top and what pinned it, the genesis, the table's keccak256, the source
client, and `extended_from` for an extended table.
- **Extending:** an extension must reproduce the old last entry and pass through the old top.
  `--out` must be another file; the old pair is never changed.
- **Handing it to Myotis:** give the table, its sidecar and its sha256 and keccak256 to
  Myotis's issue (VesperBats/myotis#899). There it becomes a pinned asset, checked against its
  keccak256 when loaded.

## 9. bee-factory

```sh
node ~/tools/bee-factory-patched/dist/cli.js start --tag v2.8.3   # the patched ports
curl -s localhost:1733/status | python3 -m json.tool                # beeMode full, 4 peers
curl -s localhost:1735/wallet | python3 -c 'import json,sys; print(json.load(sys.stdin)["chainID"])'   # 1337
```

**bee-factory is not durable: a reboot wipes its chain and every chunk.** After one:
1. Start it again.
2. Re-upload each export with `addr-upload <export> --from-start --batch <a batch of the new
   chain>`; the old batch IDs are gone.
3. Restart the follow and upload units of step 5.
4. Read the queen's peer id again (`curl localhost:1733/addresses`) for anything that dials
   it, such as Myotis's Swarm node.

## Open, as of 2026-11-20

- **Per-slice resume for the backfill,** so a multi-day import survives an interruption.
- **`--follow` with a frame index** (an export that keeps up, and an import that re-reads
  its files).
- **Postage 14b:**
  - `--batch` required on every node but bee-factory, with no fallback to a node's first
    usable batch;
  - an upload refused when it does not fit its batch;
  - every batch an index uses recorded.
- **A Bee node on Swarm mainnet,** and its funding, for publishing the full index.
