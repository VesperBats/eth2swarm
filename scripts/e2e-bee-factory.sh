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
  ACT=$($BIN devnet-activity --key "$KEY" --authority-key "$AUTH_KEY" --busy "$BUSY" \
    --transfers 120) || { echo "FAIL: devnet-activity"; exit 1; }
  echo "$ACT"
  EMITTER=$(echo "$ACT" | awk '{for (i = 1; i < NF; i++) if ($i == "emitter") print $(i + 1)}' | tr -d ,)
  echo "$ACT" | grep -q 'authority 0x' || { echo "FAIL: the chain did not take the EIP-7702 transaction"; exit 1; }
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
  # building even large slices in memory is fine here (--max-level 16), only slower on an
  # old stack.
  OUT=$($BIN --bee "$UPLOAD_BEE" addr-import --key "$ACT_KEY" --state "$ACT_STATE" --init \
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
  for A in "$BUSY" "$EMITTER" "$AUTHORITY" 0x9999999999999999999999999999999999999999; do
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
    esac
  done
  [ "${N:-0}" -ge 120 ] || { echo "FAIL: busy address has ${N:-0} appearances, expected >= 120"; exit 1; }
  BUSY_FILES=$(echo "$BUSY_OUT" | head -1 | awk '{for (i = 1; i < NF; i++) if ($(i + 1) == "files") print $i}')
  [ "${BUSY_FILES:-0}" -ge 1 ] || { echo "FAIL: the busy address did not spill: $BUSY_OUT"; exit 1; }
  # The full lookup of the (fresh) busy address finds exactly what the incremental one did.
  FULL=$(lookup "$BUSY" | count)
  [ "$FULL" = "$N" ] || { echo "FAIL: the full lookup found $FULL entries, --since $U1 found $N"; exit 1; }
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
  echo "ok: busy address ($N appearances, $BUSY_FILES spill file), emitter, authority and an unrelated address match the chain since unit $U1; the full lookup agrees; the sender's incremental lookup reads $SINCE_SLICES of $FULL_SLICES slices; head discovery finds $ACT_UNITS units"

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
  OUT=$($BIN --bee "$UPLOAD_BEE" addr-import --key "$LATE_KEY" --state "$LATE_STATE" --init \
    --from-block "$LATE_FROM" --units "$ACT_UNITS" "${TRACE_FLAG[@]}") \
    || { echo "FAIL: addr-import --from-block $LATE_FROM"; exit 1; }
  LATE_OWNER=$(echo "$OUT" | awk '/^Address index of owner/ {print $5}')
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
    --from-block 0 --units "$ACT_UNITS" 2>&1); then
    echo "FAIL: addr-import moved the start of an existing index"; exit 1
  fi
  echo "$OUT" | grep -q "cannot move it" \
    || { echo "FAIL: moving the start failed for another reason: $OUT"; exit 1; }
  rm -rf "$LATE_STATE"
  echo "ok: the index from block $((S * 64)) verifies, holds the busy address's $N entries and the sender's $LATE_SENDER since its start, matches the chain from there, and keeps its start"
fi

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

echo "== all checks passed (anchor $ANCHOR)"
