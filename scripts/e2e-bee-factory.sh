#!/usr/bin/env bash
# End-to-end test of the SOC block store against a running bee-factory stack.
#
#   bee-factory start --tag v2.8.3
#   scripts/e2e-bee-factory.sh
#
# Reads Ethereum mainnet blocks from the zbox node (override with ETH2SWARM_RPC), imports them
# via worker 1 (which holds the postage batch), then reads and verifies them through the queen
# (proves p2p retrieval), checks that bad inputs are rejected, exercises the address index
# (detection, slices, import with deferred merges and crash recovery, verification, and on a
# dev chain lookups checked against the chain), and checks that the inspector
# (`ethswarm serve`) verifies a block too.
#
# Against the local Anvil chain instead (what the integration workflow does):
#   ETH2SWARM_RPC=http://localhost:8545 scripts/e2e-bee-factory.sh
#
# Bee API URLs default to bee-factory's ports; with the port patch
# (scripts/patch-bee-factory-ports.sh) pass the patched ones:
#   UPLOAD_BEE=http://localhost:1735 READ_BEE=http://localhost:1733 \
#     ETH2SWARM_RPC=http://localhost:8546 scripts/e2e-bee-factory.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# SOC owner key: Anvil dev account #0 — a public Foundry test key, only for the dev stack.
KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
OWNER=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
EPOCH_SIZE=${EPOCH_SIZE:-64}
EPOCHS=${EPOCHS:-4}
# Worker 1 holds the postage batch; reading through the queen proves p2p retrieval.
UPLOAD_BEE=${UPLOAD_BEE:-http://localhost:1635}
READ_BEE=${READ_BEE:-http://localhost:1633}
INSPECTOR_ADDR=${INSPECTOR_ADDR:-127.0.0.1:8099}

export ETH2SWARM_RPC=${ETH2SWARM_RPC:-http://zbox.tail2ab6c8.ts.net:8561}
export RUST_LOG=${RUST_LOG:-warn}
cargo build -q --locked
BIN=target/debug/ethswarm
LAST=$((EPOCH_SIZE * EPOCHS - 1))
# The address-index steps read blocks 0..=319 (five units), which must be finalized: the
# Anvil node finalizes 64 blocks behind its tip. They may lie beyond the imported range.
NEED=$((LAST > 383 ? LAST : 383))

# A freshly started Anvil chain may not have the blocks yet (1 block per second). A
# failed `tip` call is retried; output that has no numeric tip fails at once. This loop
# is the first RPC access, so the chain ID is taken from its last output, not from a
# separate, unretried call.
TIP=-1
for ((i = 0; i < 600; i++)); do
  if TIP_OUT=$($BIN tip 2>&1); then
    TIP=$(printf '%s\n' "$TIP_OUT" | awk '/Tip:/ {sub("#", "", $2); print $2}')
    [[ "$TIP" =~ ^[0-9]+$ ]] || { echo "FAIL: no chain tip in: $TIP_OUT"; exit 1; }
    [ "$TIP" -ge "$NEED" ] && break
  fi
  [ "$i" -eq 0 ] && echo "== waiting for block #$NEED (tip #$TIP)"
  sleep 1
done
[ "$TIP" -ge "$NEED" ] || { echo "FAIL: chain tip #$TIP never reached #$NEED"; exit 1; }
CHAIN_ID=$(printf '%s\n' "$TIP_OUT" | awk '/Chain ID:/ {print $3}')
[[ "$CHAIN_ID" =~ ^[0-9]+$ ]] || { echo "FAIL: no chain ID in: $TIP_OUT"; exit 1; }

echo "== import $EPOCHS epochs × $EPOCH_SIZE blocks via $UPLOAD_BEE"
OUT=$($BIN --bee "$UPLOAD_BEE" import --key "$KEY" --epoch-size "$EPOCH_SIZE" --epochs "$EPOCHS")
echo "$OUT"
ANCHOR=$(echo "$OUT" | awk '/Trust anchor:/ {print $3}')

client() { $BIN --bee "$READ_BEE" "$@" --owner "$OWNER" --chain-id "$CHAIN_ID" --anchor "$ANCHOR"; }

echo "== verify blocks via $READ_BEE"
for n in 0 3 $((LAST / 2)) "$LAST"; do client get "$n"; done

echo "== verify every epoch via $READ_BEE"
for ((k = 0; k < EPOCHS; k++)); do client verify-epoch "$k"; done

expect_fail() {
  local what=$1; shift
  if "$@" >/dev/null 2>&1; then echo "FAIL: $what was accepted"; exit 1; fi
  echo "ok: $what rejected"
}

echo "== negative tests"
BAD=0x$(printf '11%.0s' {1..32})
expect_fail "wrong anchor" $BIN --bee "$READ_BEE" get 3 --owner "$OWNER" \
  --chain-id "$CHAIN_ID" --anchor "$BAD"
expect_fail "wrong owner" $BIN --bee "$READ_BEE" get 3 \
  --owner 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 --chain-id "$CHAIN_ID" --anchor "$ANCHOR"
expect_fail "block outside accumulator" client get $((LAST + 1))

echo "== address appearances (docs/address-index.md, detection rules)"
# The first imported block with a transaction; with traces if the RPC serves trace_block
# (Anvil, Erigon, Reth; not geth). The block, receipts and traces are verified on the way.
TX_BLOCK=
TRACE_FLAG=()
for ((n = 0; n <= LAST; n++)); do
  TXS=$($BIN block "$n" | awk '/Txs:/ {print $2}')
  [[ "$TXS" =~ ^[0-9]+$ ]] || { echo "FAIL: no transaction count for block $n"; exit 1; }
  if [ "$TXS" -gt 0 ]; then TX_BLOCK=$n; break; fi
done
if [ -z "$TX_BLOCK" ]; then
  echo "skip: no transaction in blocks 0..$LAST"
else
  $BIN appearances "$TX_BLOCK" --traces >/dev/null 2>&1 && TRACE_FLAG=(--traces)
  APPS=$($BIN appearances "$TX_BLOCK" "${TRACE_FLAG[@]}") \
    || { echo "FAIL: appearances of block $TX_BLOCK"; exit 1; }
  echo "$APPS" | tail -1
  echo "$APPS" | grep -Eq '[[:space:]]from(\||$)' \
    || { echo "FAIL: block $TX_BLOCK has a transaction but no FROM appearance"; exit 1; }
  if [ ${#TRACE_FLAG[@]} -gt 0 ]; then
    echo "$APPS" | grep -q 'trace' \
      || { echo "FAIL: block $TX_BLOCK traced, but no TRACE appearance"; exit 1; }
  fi
  echo "ok: appearances of block $TX_BLOCK${TRACE_FLAG:+ (with traces)}"
fi

echo "== address-index slice (0, 0) built twice: identical buckets root"
slice_root() { $BIN addr-slice 0 0 "${TRACE_FLAG[@]}" | awk '/buckets root:/ {print $3}'; }
ROOT_A=$(slice_root) && ROOT_B=$(slice_root) || { echo "FAIL: addr-slice 0 0"; exit 1; }
[[ "$ROOT_A" =~ ^0x[0-9a-f]{64}$ ]] || { echo "FAIL: no buckets root from addr-slice: $ROOT_A"; exit 1; }
[ "$ROOT_A" = "$ROOT_B" ] || { echo "FAIL: addr-slice 0 0 gave $ROOT_A, then $ROOT_B"; exit 1; }
echo "ok: slice (0, 0) buckets root $ROOT_A"

echo "== address-index sizes (addr-stats) of the level-1 window at block 0"
STATS=$($BIN addr-stats --level 1 --at 0 "${TRACE_FLAG[@]}") || { echo "FAIL: addr-stats"; exit 1; }
ROWS=$(printf '%s\n' "$STATS" | grep -cE '^\| [01] \|' || true)
[ "$ROWS" = 2 ] || { echo "FAIL: addr-stats printed $ROWS level rows, expected 2:"; echo "$STATS"; exit 1; }
echo "ok: addr-stats"

echo "== address index: import, resume with deferred merges, verify via $READ_BEE"
# A fresh key per run: the origin and the heads are write-once SOCs, and a rerun against the
# same stack must not collide with an earlier run's.
ADDR_KEY=0x$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
ADDR_STATE=$(mktemp -d)
addr_import() {
  $BIN --bee "$UPLOAD_BEE" addr-import --key "$ADDR_KEY" --state "$ADDR_STATE" \
    --defer-level 1 "${TRACE_FLAG[@]}" "$@"
}
addr_verify() {
  $BIN --bee "$READ_BEE" addr-verify --owner "$ADDR_OWNER" --chain-id "$CHAIN_ID" --units "$1"
}
OUT=$(addr_import --units 2 --init) || { echo "FAIL: addr-import (backfill)"; echo "$OUT"; exit 1; }
ADDR_OWNER=$(echo "$OUT" | awk '/^Address index of owner/ {print $5}')
[[ "$ADDR_OWNER" =~ ^0x[0-9a-fA-F]{40}$ ]] || { echo "FAIL: no owner in: $OUT"; exit 1; }
# Another first run under the same key, from an empty state directory, must be refused: it
# would write a second origin.
OTHER_STATE=$(mktemp -d)
if $BIN --bee "$UPLOAD_BEE" addr-import --key "$ADDR_KEY" --state "$OTHER_STATE" --init \
    --units 2 >/dev/null 2>&1; then
  echo "FAIL: a second --init under the same key was accepted"; exit 1
fi
rm -rf "$OTHER_STATE"
echo "ok: a second backfill under the same key is refused"
# Unit 4 completes slice (2, 0), which is deferred: head 4 lists it as pending.
addr_import --units 4 >/dev/null || { echo "FAIL: addr-import (resume to 4 units)"; exit 1; }
for u in 2 3 4; do
  OUT=$(addr_verify "$u") || { echo "FAIL: addr-verify --units $u"; echo "$OUT"; exit 1; }
  echo "$OUT" | tail -1
done
echo "$OUT" | grep -q 'pending \[(2, 0)\]' \
  || { echo "FAIL: head 4 does not list the deferred merge (2, 0)"; exit 1; }
# A client reading head 4 expands the pending merge; its result must match the chain.
$BIN --bee "$READ_BEE" addr-lookup "$OWNER" --owner "$ADDR_OWNER" --chain-id "$CHAIN_ID" \
  --units 4 --limit 0 --against-chain >/dev/null \
  || { echo "FAIL: addr-lookup against head 4 (pending (2, 0)) differs from the chain"; exit 1; }
echo "ok: a lookup through the pending merge's expansion matches the chain"
# Unit 5, with two crashes: one between saving head 5 and uploading it, one after the upload
# but before the state records it. Each next run must publish exactly the prepared head (the
# second one uploads an identical SOC again). Head 5 drops the built merge (2, 0).
for fp in before-head-upload after-head-upload; do
  if ETH2SWARM_ADDR_FAILPOINT=$fp addr_import --units 5 >/dev/null 2>&1; then
    echo "FAIL: failpoint $fp did not stop the run"; exit 1
  fi
done
addr_import --units 5 >/dev/null || { echo "FAIL: addr-import (resume to 5 units after crashes)"; exit 1; }
echo "ok: head 5 published after a crash before and one after its upload"
OUT=$(addr_verify 5) || { echo "FAIL: addr-verify --units 5"; echo "$OUT"; exit 1; }
echo "$OUT" | tail -1
PUBLISHED=$(echo "$OUT" | awk '/slice \(2, 0\)/ {print $NF}')
LOCAL=$($BIN addr-slice 2 0 "${TRACE_FLAG[@]}" | awk '/buckets root:/ {print $3}')
[ -n "$PUBLISHED" ] && [ "$PUBLISHED" = "$LOCAL" ] \
  || { echo "FAIL: published (2, 0) root '$PUBLISHED', local build '$LOCAL'"; exit 1; }
rm -rf "$ADDR_STATE"
echo "ok: address index published, resumed, merged and verified (slice (2, 0) root $LOCAL)"

echo "== address index from block 64: a backfill stopped and resumed, a deferred merge the start falls into"
LM_KEY=0x$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
LM_STATE=$(mktemp -d)
lm_import() {
  $BIN --bee "$UPLOAD_BEE" addr-import --key "$LM_KEY" --state "$LM_STATE" \
    --defer-level 1 "${TRACE_FLAG[@]}" "$@"
}
lm_verify() {
  $BIN --bee "$READ_BEE" addr-verify --owner "$LM_OWNER" --chain-id "$CHAIN_ID" --units "$1"
}
# The backfill stops after its first slice; the next run, without --from-block, must finish
# the same backfill from block 64, not start another from genesis.
if ETH2SWARM_ADDR_FAILPOINT=backfill-slice lm_import --init --from-block 64 --units 2 >/dev/null 2>&1; then
  echo "FAIL: failpoint backfill-slice did not stop the backfill"; exit 1
fi
OUT=$(lm_import --init --units 2) || { echo "FAIL: resuming the stopped backfill"; echo "$OUT"; exit 1; }
LM_OWNER=$(echo "$OUT" | awk '/^Address index of owner/ {print $5}')
# Unit 4 defers (2, 0), which holds units 1..4 here and is built with no parent; unit 5's
# head drops it.
lm_import --units 4 >/dev/null || { echo "FAIL: late index to 4 units"; exit 1; }
for u in 2 3 4; do
  OUT=$(lm_verify "$u") || { echo "FAIL: addr-verify of the late index, --units $u"; echo "$OUT"; exit 1; }
  echo "$OUT" | tail -1 | grep -q "from block 64 " || { echo "FAIL: the late index does not start at block 64: $OUT"; exit 1; }
done
echo "$OUT" | grep -q 'pending \[(2, 0)\]' || { echo "FAIL: head 4 of the late index does not list (2, 0)"; exit 1; }
$BIN --bee "$READ_BEE" addr-lookup "$OWNER" --owner "$LM_OWNER" --chain-id "$CHAIN_ID" \
  --units 4 --limit 0 --against-chain >/dev/null \
  || { echo "FAIL: a lookup in the late index through the pending merge differs from the chain"; exit 1; }
lm_import --units 5 >/dev/null || { echo "FAIL: late index to 5 units"; exit 1; }
OUT=$(lm_verify 5) || { echo "FAIL: addr-verify of the late index, --units 5"; echo "$OUT"; exit 1; }
echo "$OUT" | tail -1
echo "$OUT" | grep -q 'pending \[\]' || { echo "FAIL: head 5 of the late index still lists a merge"; exit 1; }
$BIN --bee "$READ_BEE" addr-lookup "$OWNER" --owner "$LM_OWNER" --chain-id "$CHAIN_ID" \
  --units 5 --limit 0 --against-chain >/dev/null \
  || { echo "FAIL: a lookup in the late index through the merged (2, 0) differs from the chain"; exit 1; }
rm -rf "$LM_STATE"
echo "ok: the index from block 64 resumed its stopped backfill with its start, deferred and built (2, 0) from unit 1, and matches the chain"

echo "== address index exported to a directory (--out) without a Bee node, uploaded later (addr-upload)"
EXP_KEY=0x$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
EXP_STATE=$(mktemp -d)
EXP_DIR=$(mktemp -d)/export
# No Bee node: the export must not need one (port 9 refuses connections).
# A sort buffer of four appearances: every slice is built from many runs on disk, merged in
# rounds, and must still match the in-memory build (slice (2, 0) below).
exp_import() {
  ETH2SWARM_ADDR_SORT_BUFFER_BYTES=180 $BIN --bee http://127.0.0.1:9 addr-import --key "$EXP_KEY" --state "$EXP_STATE" \
    --out "$EXP_DIR" --defer-level 1 "${TRACE_FLAG[@]}" "$@"
}
exp_upload() { $BIN --bee "$UPLOAD_BEE" addr-upload "$EXP_DIR" "$@"; }
OUT=$(exp_import --units 2 --init) || { echo "FAIL: addr-import --out (backfill)"; echo "$OUT"; exit 1; }
EXP_OWNER=$(echo "$OUT" | awk '/^Address index of owner/ {print $5}')
# Crashes around head 3 (in an export: before and after it is written to the log): the next
# run truncates the log to its state and writes the same records again.
for fp in before-head-upload after-head-upload; do
  if ETH2SWARM_ADDR_FAILPOINT=$fp exp_import --units 3 >/dev/null 2>&1; then
    echo "FAIL: failpoint $fp did not stop the export"; exit 1
  fi
done
exp_import --units 4 >/dev/null || { echo "FAIL: addr-import --out (resume to 4 units)"; exit 1; }
# An exported index cannot continue as an uploaded one.
if OUT=$($BIN --bee "$UPLOAD_BEE" addr-import --key "$EXP_KEY" --state "$EXP_STATE" --units 4 \
  "${TRACE_FLAG[@]}" 2>&1); then
  echo "FAIL: an exported index continued without --out"; exit 1
fi
echo "$OUT" | grep -q "exported to a directory" || { echo "FAIL: unexpected refusal: $OUT"; exit 1; }
OUT=$(exp_upload) || { echo "FAIL: addr-upload"; echo "$OUT"; exit 1; }
echo "$OUT"
for u in 2 3 4; do
  OUT=$($BIN --bee "$READ_BEE" addr-verify --owner "$EXP_OWNER" --chain-id "$CHAIN_ID" --units "$u") \
    || { echo "FAIL: addr-verify of the uploaded export, --units $u"; echo "$OUT"; exit 1; }
done
# Nothing new: a second upload resumes at the end and sends nothing.
exp_upload | grep -q ": 0 files, 0 chunks of large files, 0 SOCs, 0 heads" || { echo "FAIL: a repeated upload sent records again"; exit 1; }
# Another batch does not resume what the first one stamped.
if OUT=$(exp_upload --batch 0x$(printf '01%.0s' {1..32}) 2>&1); then
  echo "FAIL: an upload with another batch resumed the progress of the first"; exit 1
fi
echo "$OUT" | grep -q -- "--from-start" || { echo "FAIL: unexpected refusal: $OUT"; exit 1; }
# --from-start sends everything again (identical records, the origin check passes on our own
# origin).
OUT=$(exp_upload --from-start) || { echo "FAIL: addr-upload --from-start"; echo "$OUT"; exit 1; }
echo "$OUT" | grep -q ", 4 heads" || { echo "FAIL: --from-start did not send every head again: $OUT"; exit 1; }
# Another key's export cannot be uploaded over this index: a second export under the same key
# from genesis with another start is refused at its origin.
DUP_STATE=$(mktemp -d); DUP_DIR=$(mktemp -d)/export
$BIN --bee http://127.0.0.1:9 addr-import --key "$EXP_KEY" --state "$DUP_STATE" --out "$DUP_DIR" \
  --init --from-block 64 --units 3 "${TRACE_FLAG[@]}" >/dev/null || { echo "FAIL: second export"; exit 1; }
if OUT=$($BIN --bee "$UPLOAD_BEE" addr-upload "$DUP_DIR" 2>&1); then
  echo "FAIL: an export under a key with another published index was uploaded"; exit 1
fi
echo "$OUT" | grep -q "already published an index" || { echo "FAIL: unexpected refusal: $OUT"; exit 1; }
rm -rf "$DUP_STATE" "$(dirname "$DUP_DIR")"
# More units, uploaded incrementally; the merged slice (2, 0) is the one built directly.
exp_import --units 5 >/dev/null || { echo "FAIL: addr-import --out (5 units)"; exit 1; }
OUT=$(exp_upload) || { echo "FAIL: addr-upload (incremental)"; echo "$OUT"; exit 1; }
echo "$OUT" | grep -q ", 1 heads" || { echo "FAIL: the incremental upload did not send exactly head 5: $OUT"; exit 1; }
OUT=$($BIN --bee "$READ_BEE" addr-verify --owner "$EXP_OWNER" --chain-id "$CHAIN_ID" --units 5) \
  || { echo "FAIL: addr-verify of the uploaded export, --units 5"; echo "$OUT"; exit 1; }
EXPORTED=$(echo "$OUT" | awk '/slice \(2, 0\)/ {print $NF}')
[ "$EXPORTED" = "$LOCAL" ] || { echo "FAIL: exported (2, 0) root '$EXPORTED', local build '$LOCAL'"; exit 1; }
rm -rf "$EXP_STATE" "$(dirname "$EXP_DIR")"
# One directory for state, export and sort runs works too (a new export there is fresh).
ONE_DIR=$(mktemp -d)
ETH2SWARM_ADDR_SORT_BUFFER_BYTES=180 $BIN --bee http://127.0.0.1:9 addr-import \
  --key 0x$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n') --state "$ONE_DIR" --out "$ONE_DIR" \
  --init --units 2 "${TRACE_FLAG[@]}" >/dev/null \
  || { echo "FAIL: an export into its own state directory"; exit 1; }
rm -rf "$ONE_DIR"
echo "ok: an index exported without a Bee node, through two crashes, uploads, resumes its upload and verifies (slice (2, 0) root $EXPORTED)"

if [ "$CHAIN_ID" = 1337 ] || [ "$CHAIN_ID" = 31337 ]; then
  echo "== address index with traffic: devnet-activity, import, lookups checked against the chain"
  # Public Foundry dev keys #0 (sender) and #2 (EIP-7702 authority), dev chains only.
  AUTH_KEY=0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a
  AUTHORITY=0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC
  BUSY=0x$(od -An -tx1 -N20 /dev/urandom | tr -d ' \n')
  # Start right after a unit boundary, so that the activity (about seven blocks at one block
  # per second) stays inside one unit: the import then ends with that unit, and the wait for
  # its finality is at most about two units.
  for ((i = 0; i < 120; i++)); do
    T=$($BIN tip 2>/dev/null | awk '/Tip:/ {sub("#", "", $2); print $2}') || true
    [[ "$T" =~ ^[0-9]+$ ]] && [ $((T % 64)) -le 40 ] && break
    sleep 1
  done
  # 1,400 transfers: the busy address's spill file has two pages (a page holds 1,363 entries
  # of 3 bytes), so it can be streamed and read page by page below.
  ACT=$($BIN devnet-activity --key "$KEY" --authority-key "$AUTH_KEY" --busy "$BUSY" \
    --transfers 1400) || { echo "FAIL: devnet-activity"; exit 1; }
  echo "$ACT"
  EMITTER=$(echo "$ACT" | awk '{for (i = 1; i < NF; i++) if ($i == "emitter") print $(i + 1)}' | tr -d ,)
  echo "$ACT" | grep -q 'authority 0x' || { echo "FAIL: the chain did not take the EIP-7702 transaction"; exit 1; }
  FAILED=$(echo "$ACT" | awk '{for (i = 1; i < NF; i++) if ($i == "failed" && $(i + 1) == "creation") print $(i + 2)}')
  [[ "$FAILED" =~ ^0x[0-9a-fA-F]{40}$ ]] || { echo "FAIL: no failed creation in: $ACT"; exit 1; }
  ACT_RANGE=$(echo "$ACT" | awk '{print $4}' | tr -d :)
  ACT_FIRST=${ACT_RANGE%%..=*}
  ACT_LAST=${ACT_RANGE##*..=}
  [[ "$ACT_FIRST" =~ ^[0-9]+$ && "$ACT_LAST" =~ ^[0-9]+$ ]] || { echo "FAIL: no block range in: $ACT"; exit 1; }
  # Import up to the end of the smallest aligned slice that holds all of the activity, so that
  # one slice holds every entry of the busy address and it spills deterministically.
  U1=$((ACT_FIRST / 64)); U2=$((ACT_LAST / 64)); L=0
  while [ $((U1 >> L)) -ne $((U2 >> L)) ]; do L=$((L + 1)); done
  ACT_UNITS=$((((U2 >> L) + 1) << L))
  # Anvil finalizes 64 blocks behind its tip.
  WANT=$((ACT_UNITS * 64 - 1 + 64))
  for ((i = 0; i < 900; i++)); do
    T=$($BIN tip 2>/dev/null | awk '/Tip:/ {sub("#", "", $2); print $2}') || true
    [[ "$T" =~ ^[0-9]+$ ]] && [ "$T" -ge "$WANT" ] && break
    sleep 1
  done
  [ "${T:-0}" -ge "$WANT" ] || { echo "FAIL: tip never reached #$WANT"; exit 1; }
  ACT_KEY=0x$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
  ACT_STATE=$(mktemp -d)
  # The backfill covers the whole dev chain from genesis; its blocks are nearly empty, so
  # even large slices are quick to build here (--max-level 16, above the default of 12), only
  # slower on an old stack. Every spill file is streamed: uploaded chunk by chunk as it is
  # written, as a busy address of a top-level slice is.
  OUT=$(ETH2SWARM_ADDR_STREAM_FILE_BYTES=1 $BIN --bee "$UPLOAD_BEE" addr-import --key "$ACT_KEY" --state "$ACT_STATE" --init \
    --units "$ACT_UNITS" --max-level 16 "${TRACE_FLAG[@]}") \
    || { echo "FAIL: addr-import $ACT_UNITS units"; exit 1; }
  ACT_OWNER=$(echo "$OUT" | awk '/^Address index of owner/ {print $5}')
  OUT=$($BIN --bee "$READ_BEE" addr-verify --owner "$ACT_OWNER" --chain-id "$CHAIN_ID" \
    --units "$ACT_UNITS") || { echo "FAIL: addr-verify"; echo "$OUT"; exit 1; }
  echo "$OUT" | tail -1
  lookup() {
    $BIN --bee "$READ_BEE" addr-lookup "$1" --owner "$ACT_OWNER" --chain-id "$CHAIN_ID" \
      --units "$ACT_UNITS" --limit 0 "${@:2}"
  }
  count() { awk '/appearances in the index/ {print $2}'; }
  # Checked against the chain from the activity's first unit on: re-detecting the whole chain
  # would grow with its age (the authority key has appearances from earlier runs too).
  for A in "$BUSY" "$EMITTER" "$AUTHORITY" "$FAILED" 0x9999999999999999999999999999999999999999; do
    OUT=$(lookup "$A" --since "$U1" --against-chain) \
      || { echo "FAIL: addr-lookup $A"; echo "$OUT"; exit 1; }
    echo "$OUT" | head -1
    case "$A" in
      "$BUSY") N=$(echo "$OUT" | count); BUSY_OUT=$OUT ;;
      "$AUTHORITY")
        NEWEST=$($BIN --bee "$READ_BEE" addr-lookup "$A" --owner "$ACT_OWNER" \
          --chain-id "$CHAIN_ID" --units "$ACT_UNITS" --since "$U1" --limit 1)
        echo "$NEWEST" | head -1 | grep -qw auth \
          || { echo "FAIL: the authority's newest entry has no AUTH role: $NEWEST"; exit 1; } ;;
      "$FAILED")
        # A reverted top-level creation: its target is TO, and with traces also TRACE.
        ENTRY=$($BIN --bee "$READ_BEE" addr-lookup "$A" --owner "$ACT_OWNER" \
          --chain-id "$CHAIN_ID" --units "$ACT_UNITS" --since "$U1" --limit 1)
        ENTRY=$(echo "$ENTRY" | head -1)
        echo "$ENTRY" | grep -qw to \
          || { echo "FAIL: the failed creation's target has no TO role: $OUT"; exit 1; }
        if [ ${#TRACE_FLAG[@]} -gt 0 ]; then
          echo "$ENTRY" | grep -qw trace \
            || { echo "FAIL: the failed creation's target has no TRACE role: $OUT"; exit 1; }
        fi ;;
    esac
  done
  [ "${N:-0}" -ge 1400 ] || { echo "FAIL: busy address has ${N:-0} appearances, expected >= 1400"; exit 1; }
  BUSY_FILES=$(echo "$BUSY_OUT" | head -1 | awk '{for (i = 1; i < NF; i++) if ($(i + 1) == "files") print $i}')
  [ "${BUSY_FILES:-0}" -ge 1 ] || { echo "FAIL: the busy address did not spill: $BUSY_OUT"; exit 1; }
  # The full lookup of the (fresh) busy address finds exactly what the incremental one did.
  FULL=$(lookup "$BUSY" | count)
  [ "$FULL" = "$N" ] || { echo "FAIL: the full lookup found $FULL entries, --since $U1 found $N"; exit 1; }
  # Read page by page through the chunk tree, as a spill file above the whole-file limit is
  # (a test limit of one page): the same entries, matching the chain; and the whole index
  # verifies that way too.
  OUT=$(ETH2SWARM_ADDR_WHOLE_FILE_BYTES=4096 lookup "$BUSY" --since "$U1" --against-chain) \
    || { echo "FAIL: addr-lookup of the busy address page by page"; echo "$OUT"; exit 1; }
  PAGED=$(echo "$OUT" | count)
  [ "$PAGED" = "$N" ] || { echo "FAIL: page by page the busy address has $PAGED entries, not $N"; exit 1; }
  ETH2SWARM_ADDR_WHOLE_FILE_BYTES=4096 $BIN --bee "$READ_BEE" addr-verify --owner "$ACT_OWNER" \
    --chain-id "$CHAIN_ID" --units "$ACT_UNITS" >/dev/null \
    || { echo "FAIL: addr-verify reading spill files page by page"; exit 1; }
  # The sender has appearances before and after unit U1: the incremental lookup must read
  # fewer slices than the full one and match the chain from U1 on.
  slices_read() { awk '/appearances in the index/ {for (i = 1; i < NF; i++) if ($(i + 1) == "slices,") print $i}'; }
  FULL_SLICES=$(lookup "$OWNER" | slices_read)
  OUT=$(lookup "$OWNER" --since "$U1" --against-chain) \
    || { echo "FAIL: incremental lookup of the sender"; echo "$OUT"; exit 1; }
  SINCE_SLICES=$(echo "$OUT" | slices_read)
  if [ "${FULL_SLICES:-0}" -gt 1 ] && [ "${SINCE_SLICES:-0}" -ge "$FULL_SLICES" ]; then
    echo "FAIL: --since $U1 read $SINCE_SLICES slices, the full lookup $FULL_SLICES"; exit 1
  fi
  # Head discovery (no --units): the newest head of this index is the one for ACT_UNITS.
  FOUND_UNITS=$($BIN --bee "$READ_BEE" addr-lookup "$BUSY" --owner "$ACT_OWNER" \
    --chain-id "$CHAIN_ID" --limit 0 2>/dev/null | sed -n 's/.*(\([0-9]*\) units.*/\1/p')
  [ "$FOUND_UNITS" = "$ACT_UNITS" ] \
    || { echo "FAIL: head discovery found $FOUND_UNITS units, the index has $ACT_UNITS"; exit 1; }
  rm -rf "$ACT_STATE"
  echo "ok: busy address ($N appearances, $BUSY_FILES spill file), emitter, authority, a failed creation's target and an unrelated address match the chain since unit $U1; the full lookup agrees; the sender's incremental lookup reads $SINCE_SLICES of $FULL_SLICES slices; head discovery finds $ACT_UNITS units"

  echo "== address index from a later block (--from-block): import, verify, lookups checked against the chain"
  # Start one unit into the newest slice of the cover (at most unit U1) that is longer than one
  # unit, so that the index's start falls inside it: that slice then holds its units from the
  # start on. Without such a slice the start is unit U1 itself.
  # The cover of ACT_UNITS units is one slice per set bit (a dev chain is shorter than one
  # top-level slice of 2^16 units).
  [ "$ACT_UNITS" -lt 65536 ] || { echo "FAIL: dev chain of $ACT_UNITS units, longer than expected"; exit 1; }
  S=$U1; START=0
  for ((LV = 15; LV >= 0; LV--)); do
    [ $(((ACT_UNITS >> LV) & 1)) = 1 ] || continue
    if [ "$LV" -ge 1 ] && [ "$START" -lt "$U1" ]; then S=$((START + 1)); fi
    START=$((START + (1 << LV)))
  done
  LATE_FROM=$((S * 64 + 10))
  LATE_KEY=0x$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
  LATE_STATE=$(mktemp -d)
  LATE_DIR=$(mktemp -d)/export
  # Exported and uploaded later: this index has spill files, streamed into the log as their
  # chunks (records of kind CHUNK), so they take that path too.
  OUT=$(ETH2SWARM_ADDR_SORT_BUFFER_BYTES=180 ETH2SWARM_ADDR_STREAM_FILE_BYTES=1 $BIN --bee http://127.0.0.1:9 addr-import --key "$LATE_KEY" --state "$LATE_STATE" --init \
    --from-block "$LATE_FROM" --units "$ACT_UNITS" --max-level 16 --out "$LATE_DIR" "${TRACE_FLAG[@]}") \
    || { echo "FAIL: addr-import --from-block $LATE_FROM --out"; exit 1; }
  LATE_OWNER=$(echo "$OUT" | awk '/^Address index of owner/ {print $5}')
  OUT=$($BIN --bee "$UPLOAD_BEE" addr-upload "$LATE_DIR") || { echo "FAIL: addr-upload of the late index"; echo "$OUT"; exit 1; }
  echo "$OUT"
  echo "$OUT" | grep -q " 0 chunks of large files" && { echo "FAIL: the late index exported no streamed spill file: $OUT"; exit 1; }
  OUT=$($BIN --bee "$READ_BEE" addr-verify --owner "$LATE_OWNER" --chain-id "$CHAIN_ID" \
    --units "$ACT_UNITS") || { echo "FAIL: addr-verify of the late index"; echo "$OUT"; exit 1; }
  echo "$OUT" | tail -1
  echo "$OUT" | tail -1 | grep -q "from block $((S * 64)) " \
    || { echo "FAIL: the late index does not start at block $((S * 64)): $OUT"; exit 1; }
  late_lookup() {
    $BIN --bee "$READ_BEE" addr-lookup "$1" --owner "$LATE_OWNER" --chain-id "$CHAIN_ID" \
      --units "$ACT_UNITS" --limit 0 "${@:2}"
  }
  # Without --since: the whole late index, checked against the chain from its start. The
  # sender has appearances before the start, which the late index must not hold.
  for A in "$BUSY" "$OWNER"; do
    OUT=$(late_lookup "$A" --against-chain) \
      || { echo "FAIL: addr-lookup $A in the late index"; echo "$OUT"; exit 1; }
    echo "$OUT" | grep -q "The index starts at block $((S * 64))" \
      || { echo "FAIL: the lookup does not report the late start: $OUT"; exit 1; }
  done
  LATE_N=$(late_lookup "$BUSY" | count)
  [ "$LATE_N" = "$N" ] || { echo "FAIL: the late index has $LATE_N busy entries, the full one $N"; exit 1; }
  LATE_SENDER=$(late_lookup "$OWNER" | count)
  SINCE_SENDER=$(lookup "$OWNER" --since "$S" | count)
  [ "$LATE_SENDER" = "$SINCE_SENDER" ] \
    || { echo "FAIL: the late index has $LATE_SENDER sender entries, the full one $SINCE_SENDER since unit $S"; exit 1; }
  # The start is fixed: a later run cannot move it.
  if OUT=$($BIN --bee "$UPLOAD_BEE" addr-import --key "$LATE_KEY" --state "$LATE_STATE" \
    --out "$LATE_DIR" --from-block 0 --units "$ACT_UNITS" "${TRACE_FLAG[@]}" 2>&1); then
    echo "FAIL: addr-import moved the start of an existing index"; exit 1
  fi
  echo "$OUT" | grep -q "cannot move it" \
    || { echo "FAIL: moving the start failed for another reason: $OUT"; exit 1; }
  rm -rf "$LATE_STATE" "$(dirname "$LATE_DIR")"
  echo "ok: the index from block $((S * 64)) verifies, holds the busy address's $N entries and the sender's $LATE_SENDER since its start, matches the chain from there, and keeps its start"
fi

echo "== postage batches by lifetime: lower slices go to the batch that expires soonest after them"
# Two lower batches on the upload node: one at the minimum validity (about 17,500 s on
# bee-factory) and a long one. A dev chain is short, so --slot-secs 78 stretches a unit to
# 4,992 s. With no grace period, the backfill of 4 units writes (2, 0), which lives 4 more
# units (19,968 s: the long batch); units 5 and 6 write (0, 4) and (1, 2), which live 1 and
# 2 units (the short one); heads and the origin go to the permanent batch.
buy_batch() {
  curl -sf -X POST "$UPLOAD_BEE/stamps/$1/17" \
    | python3 -c 'import json, sys; print("0x" + json.load(sys.stdin)["batchID"])'
}
batch_info() {
  # Bee does not know a batch until its purchase is confirmed: "False 0" until then.
  curl -sf "$UPLOAD_BEE/stamps/${1#0x}" 2>/dev/null \
    | python3 -c 'import json, sys; d = json.load(sys.stdin); print(d["usable"], d["batchTTL"])' 2>/dev/null \
    || echo "False 0"
}
PERM=0x$(curl -sf "$UPLOAD_BEE/stamps" \
  | python3 -c 'import json, sys; print(next(s["batchID"] for s in json.load(sys.stdin)["stamps"] if s["usable"]))')
SHORT=$(buy_batch 420000000) || { echo "FAIL: buying a short batch"; exit 1; }
LONG=$(buy_batch 5000000000) || { echo "FAIL: buying a long batch"; exit 1; }
USABLE=""
for i in $(seq 1 60); do
  if [ "$(batch_info "$SHORT" | cut -d' ' -f1)" = True ] && [ "$(batch_info "$LONG" | cut -d' ' -f1)" = True ]; then
    USABLE=1; break
  fi
  sleep 2
done
[ -n "$USABLE" ] || { echo "FAIL: the bought batches $SHORT and $LONG are not usable after 120 s"; exit 1; }
read -r _ SHORT_TTL <<<"$(batch_info "$SHORT")"
read -r _ LONG_TTL <<<"$(batch_info "$LONG")"
[ "$SHORT_TTL" -gt 9984 ] && [ "$SHORT_TTL" -lt 19968 ] && [ "$LONG_TTL" -gt 19968 ] \
  || { echo "FAIL: batch TTLs $SHORT_TTL and $LONG_TTL do not bracket the slices' 9,984 and 19,968 s"; exit 1; }
LT_KEY=0x$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
LT_STATE=$(mktemp -d)
lt_import() {
  $BIN --bee "$UPLOAD_BEE" addr-import --key "$LT_KEY" --state "$LT_STATE" --batch "$PERM" \
    --lower-batch "$SHORT" --lower-batch "$LONG" --grace-days 0 --slot-secs 78 "${TRACE_FLAG[@]}" "$@"
}
# "<batch>: <count>" per batch the run stamped, sorted, as one line.
stamped() {
  awk -v p="$PERM" -v s="$SHORT" -v l="$LONG" '$1 == "batch" {
      b = tolower($2); sub(":", "", b)
      n = (b == tolower(p)) ? "permanent" : (b == tolower(s)) ? "short" : (b == tolower(l)) ? "long" : b
      print n "=" $3 }
    /went to the permanent batch/ { print "fell_back=" $1 }' | sort | tr '\n' ' '
}
OUT=$(lt_import --init --units 4) || { echo "FAIL: addr-import with lower batches"; echo "$OUT"; exit 1; }
LT_OWNER=$(echo "$OUT" | awk '/^Address index of owner/ {print $5}')
GOT=$(echo "$OUT" | stamped)
[ "$GOT" = "long=1 permanent=2 " ] \
  || { echo "FAIL: the backfill stamped '$GOT', expected (2, 0) long, head 0 and the origin permanent"; echo "$OUT"; exit 1; }
OUT=$(lt_import --units 6) || { echo "FAIL: addr-import 6 units with lower batches"; echo "$OUT"; exit 1; }
GOT=$(echo "$OUT" | stamped)
[ "$GOT" = "permanent=2 short=2 " ] \
  || { echo "FAIL: units 5 and 6 stamped '$GOT', expected two short-lived slices and two heads"; echo "$OUT"; exit 1; }
$BIN --bee "$READ_BEE" addr-verify --owner "$LT_OWNER" --chain-id "$CHAIN_ID" --units 6 >/dev/null \
  || { echo "FAIL: addr-verify of the index stamped by lifetime"; exit 1; }
# The same through an export: the log carries each record's lifetime, addr-upload chooses.
LT_EXP_STATE=$(mktemp -d)
LT_EXP=$(mktemp -d)/export
LT_EXP_KEY=0x$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
for u in "--init --units 4" "--units 6"; do
  # shellcheck disable=SC2086
  $BIN --bee http://127.0.0.1:9 addr-import --key "$LT_EXP_KEY" --state "$LT_EXP_STATE" \
    --out "$LT_EXP" $u "${TRACE_FLAG[@]}" >/dev/null \
    || { echo "FAIL: addr-import --out ($u) for the lifetime upload"; exit 1; }
done
OUT=$($BIN --bee "$UPLOAD_BEE" addr-upload "$LT_EXP" --batch "$PERM" --lower-batch "$SHORT" \
  --lower-batch "$LONG" --grace-days 0 --slot-secs 78) \
  || { echo "FAIL: addr-upload with lower batches"; echo "$OUT"; exit 1; }
GOT=$(echo "$OUT" | stamped)
case "$GOT" in
  "long="[1-9]*" permanent=4 short="[1-9]*" ") ;;
  *) echo "FAIL: the export's upload stamped '$GOT', expected (2, 0) long, units 5 and 6 short, four heads permanent"; echo "$OUT"; exit 1 ;;
esac
rm -rf "$LT_STATE" "$LT_EXP_STATE" "$(dirname "$LT_EXP")"
echo "ok: lower slices stamped by lifetime (short batch TTL ${SHORT_TTL} s, long ${LONG_TTL} s), directly and through an export; heads and the origin on the permanent batch"

echo "== inspector (ethswarm serve) verifies a block via $READ_BEE"
# INSPECTOR_ADDR is an IP:port (a socket address, not a host name). Refuse a port that
# is already taken: the checks would otherwise talk to whatever holds it.
if curl -s -o /dev/null "http://$INSPECTOR_ADDR/"; then
  echo "FAIL: something already listens on $INSPECTOR_ADDR; set INSPECTOR_ADDR to a free IP:port"
  exit 1
fi
SERVE_LOG=$(mktemp)
$BIN serve --listen "$INSPECTOR_ADDR" --node "read=$READ_BEE" >"$SERVE_LOG" 2>&1 &
SERVE_PID=$!
trap 'kill "$SERVE_PID" 2>/dev/null || true; rm -f "$SERVE_LOG"' EXIT
serve_fail() { echo "FAIL: $1"; echo "-- ethswarm serve output:"; cat "$SERVE_LOG"; exit 1; }
for ((i = 0; i < 50; i++)); do
  curl -sf "http://$INSPECTOR_ADDR/api/config" >/dev/null && break
  kill -0 "$SERVE_PID" 2>/dev/null || serve_fail "ethswarm serve exited"
  sleep 0.2
done
curl -sf "http://$INSPECTOR_ADDR/api/config" >/dev/null || serve_fail "inspector not ready on $INSPECTOR_ADDR"
BLOCK=$((LAST / 2))
RESULT=$(curl -s "http://$INSPECTOR_ADDR/api/block?node=read&owner=$OWNER&chain=$CHAIN_ID&anchor=$ANCHOR&n=$BLOCK") \
  || serve_fail "inspector request for block $BLOCK failed"
VERIFIED=$(printf '%s' "$RESULT" | python3 -c 'import json, sys; print(json.load(sys.stdin).get("verified"))')
[ "$VERIFIED" = True ] || { echo "FAIL: inspector did not verify block $BLOCK: $RESULT"; exit 1; }
echo "ok: inspector verified block $BLOCK"
FOREIGN=$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: evil.example' "http://$INSPECTOR_ADDR/api/config")
[ "$FOREIGN" = 403 ] || { echo "FAIL: inspector answered a foreign Host header with $FOREIGN"; exit 1; }
echo "ok: inspector rejected a foreign Host header"
if [ -n "${ACT_OWNER:-}" ]; then
  # The Address index tab's lookup finds what addr-lookup found for the busy address, and
  # the newest head by probing.
  RESULT=$(curl -s "http://$INSPECTOR_ADDR/api/addr/lookup?node=read&owner=$ACT_OWNER&chain=$CHAIN_ID&address=$BUSY") \
    || serve_fail "inspector address lookup failed"
  GOT=$(printf '%s' "$RESULT" | python3 -c 'import json, sys; d = json.load(sys.stdin); print(d.get("error") or "%s %s" % (d["lookup"]["count"], d["index"]["head"]["units"]))')
  [ "$GOT" = "$N $ACT_UNITS" ] || { echo "FAIL: inspector address lookup gave '$GOT', expected '$N $ACT_UNITS'"; exit 1; }
  echo "ok: inspector's address-index lookup found the busy address's $N entries at head $ACT_UNITS"
  # The slice, bucket and spill views along the busy address's path: the one slice that
  # holds all its entries, its bucket, its record and the spill file.
  jget() { python3 -c "import json, sys; d = json.load(sys.stdin); e = d.get('error'); print('ERROR ' + e if e else $1)"; }
  api() { curl -s "http://$INSPECTOR_ADDR/api/addr/$1?node=read&owner=$ACT_OWNER&chain=$CHAIN_ID&units=$ACT_UNITS&address=$BUSY&$2"; }
  REF=""
  for S in $(printf '%s' "$RESULT" | jget "' '.join('%d:%d' % tuple(s) for s in d['index']['slices'])"); do
    SL=${S%:*} SI=${S#*:}
    K=$(api slice "level=$SL&index=$SI" | jget "d['addressBucket']")
    REC=$(api bucket "level=$SL&index=$SI&k=$K" \
      | jget "' '.join(str(r.get('spillRef')) + ' ' + str(r['entries']) for r in (d['pagedRecords'] if d['bucket']['paged'] else d['bucket']['records']) if r['match'])")
    case "$REC" in ERROR*) echo "FAIL: inspector bucket $K of slice ($SL, $SI): $REC"; exit 1 ;; esac
    [ -n "$REC" ] || continue
    read -r REF CNT <<<"$REC"
    break
  done
  [ "${CNT:-}" = "$N" ] && [ "${REF#0x}" != "$REF" ] \
    || { echo "FAIL: the busy address's record in bucket $K of slice ($SL, $SI): '$REC'"; exit 1; }
  GOT=$(api spill "level=$SL&index=$SI&ref=$REF&count=$CNT&limit=3" | jget "'%s %s %s' % (d['count'], d['shown'], d['entries'][0]['block'] >= d['entries'][-1]['block'])")
  [ "$GOT" = "$N 3 True" ] || { echo "FAIL: inspector spill view gave '$GOT', expected '$N 3 True'"; exit 1; }
  echo "ok: inspector's slice ($SL, $SI), bucket $K and spill views lead to the busy address's $N entries, newest first"
  # Example addresses: the dev accounts, counted from the buckets alone. Account #0 ($OWNER)
  # sent the traffic: its count is what the full lookup at the same head finds.
  EX=$(curl -s "http://$INSPECTOR_ADDR/api/addr/examples?node=read&owner=$ACT_OWNER&chain=$CHAIN_ID" \
    | python3 -c 'import json, sys; d = json.load(sys.stdin); print(d.get("error") or "%s %s" % (d["examples"][0]["address"].lower(), d["examples"][0]["entries"]))')
  WANT_EX=$(lookup "$OWNER" | count)
  [ "$EX" = "$(echo "$OWNER" | tr 'A-F' 'a-f') $WANT_EX" ] \
    || { echo "FAIL: inspector examples gave '$EX' for dev account #0, the lookup finds $WANT_EX"; exit 1; }
  echo "ok: inspector's example addresses count $WANT_EX entries for dev account #0, as the lookup does"
fi
if [ -n "${LM_OWNER:-}" ]; then
  # Head probing across several updates (the index from block 64: updates 0-3, 5 units):
  # stepping up, a miss, bisecting; the second lookup starts from the head the first found.
  for want in "5 3 from update 0" "5 3 from update 3"; do
    GOT=$(curl -s "http://$INSPECTOR_ADDR/api/addr/lookup?node=read&owner=$LM_OWNER&chain=$CHAIN_ID" \
      | python3 -c 'import json, re, sys; d = json.load(sys.stdin)["index"]; print(d["head"]["units"], d["headUpdate"], re.search(r"from update \d+", d["headFound"]).group(0))')
    [ "$GOT" = "$want" ] || { echo "FAIL: inspector head probing gave '$GOT', expected '$want'"; exit 1; }
  done
  echo "ok: inspector's head probing found update 3 of the index from block 64, then again from its cache"
fi

echo "== all checks passed (anchor $ANCHOR)"
