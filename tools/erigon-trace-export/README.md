# erigon-trace-export

Writes addr1's `TRACE` appearances for a block range as A1TR v1 files, read
from the call-trace indexes of an Erigon datadir instead of re-executing every
block with `trace_block`. `addr-import --trace-index <dir>` reads the files.

It is a separate Go module because Erigon's index files (`.ef`, `.seg`,
recsplit) can only be read with Erigon's own Go libraries. Erigon is an
unmodified dependency, pinned to the commit of v3.8.1. The datadir it reads
must come from the same Erigon version.

## What it reads

Erigon keeps two inverted indexes, `tracesfrom` and `tracesto`: address →
the transactions in which it is the `from` or `to` of a call frame. Erigon
records a frame when the frame is entered (`CallTracer.OnEnterV2`): the
top-level call, every internal `CALL`, `CALLCODE`, `DELEGATECALL`,
`STATICCALL`, `CREATE` and `CREATE2`, and `SELFDESTRUCT` (from the destroyed
contract to its beneficiary). Frames that revert or fail are included, and a
creation is recorded with its target address whatever its outcome. The union
of the two indexes, mapped to (block, transaction index), is the output.

The tool drops:

- Erigon's two system txNums per block (the first and the last of the
  block's span: block rewards, withdrawals and system calls such as EIP-4788
  and EIP-2935), since addr1 has no role for them;
- addresses up to `0x…ffff` (the zero address and the precompiles), which
  addr1 never indexes.

Frozen data comes from the visible `.ef` files, the tail not yet in files
from the `TracesFromKeys` / `TracesToKeys` tables. Both are read through a
read-only open of the datadir, so the tool can run next to the node.

## Output: A1TR v1

One file per block range, `trace-<first>-<last>.a1tr` with ten-digit,
zero-padded block numbers, so a directory sorts in block order. Ranges are
aligned to multiples of `--blocks-per-file` and clipped to `--from`/`--to`;
every range gets a file, an empty one included.

```
header: "A1TR" ‖ version u8 = 1 ‖ first_block u64 BE ‖ last_block u64 BE ‖
        source_len u8 ‖ source (e.g. "erigon v3.8.1")
record: block u64 BE ‖ tx_index u32 BE ‖ address [20]byte      (32 bytes)
```

Records are sorted by (block, tx_index, address), without duplicates, and
carry only `TRACE`. A file is written under a temporary name and renamed when
complete.

## Build

Needs Go 1.26 or newer and a C/C++ compiler with C++20 support (gcc 10 or
newer; Erigon's dependencies use cgo):

```sh
cd tools/erigon-trace-export
CC=gcc-13 CXX=g++-13 go build -o erigon-trace-export .
go test ./...
```

## Run

```sh
./erigon-trace-export --datadir /path/to/erigon/datadir \
  --from 15000000 --to 15099999 --out /path/to/trace-index
```

- `--to` must not be above the datadir's finalized block unless
  `--allow-unfinalized` is given, and must be a block the datadir has.
- If the node builds new index or block files while the export opens the
  datadir, the export stops with "rerun" instead of writing files with a
  hole: Erigon prunes its database tables only after the file exists.
- The datadir needs `snapshots/erigondb.toml` (the node creates it at its
  first start). Erigon's loader exits the process if it finds a gap between
  the state files and the database.
- Each run scans every index file that overlaps the range once, whatever the
  range's size, and spends about 40 s opening the datadir; export large
  ranges per run rather than many small ones.
- The external sort writes to `--tmpdir` (default `<out>/.tmp`);
  `--sort-buffer-mb` (default 128) is its memory per sorted run.
- On a machine where the node runs close to its memory limit, start the
  export in its own memory-limited scope so its page cache does not push the
  node out, e.g. `systemd-run --user --scope -p MemoryHigh=3G -p MemoryMax=6G
  ./erigon-trace-export …`.

## Checked against trace_block

On Erigon v3.8.1 mainnet, every frame address of `trace_block`
(`action.from`, `action.to`, `result.address`, and `action.address` /
`action.refundAddress` of a `suicide`) per `transactionPosition`, filtered
like above, equals the export for blocks 2300000–2300009, 4000110–4000125,
15000000–15000009, 19500000–19500009 (EIP-4788 system calls),
22500000–22500009 (a type-4 transaction), 26425000–26425019 and
26453740–26453770 (the tail from the database), with one exception: a
creation that fails. `trace_block` and `callTracer` drop its target address;
the index keeps it (block 4000118, tx 11). A Geth-style JS tracer's `enter`
hook and `ctx.to` report it the way the index does.
