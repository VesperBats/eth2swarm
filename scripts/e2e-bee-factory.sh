#!/usr/bin/env bash
# End-to-end test of the SOC block store against a running bee-factory stack.
#
#   bee-factory start --tag v2.8.3
#   scripts/e2e-bee-factory.sh
#
# Reads Ethereum mainnet blocks from the zbox node (override with ETH2SWARM_RPC), imports them
# via worker 1 (which holds the postage batch), then reads and verifies them through the queen
# (proves p2p retrieval), checks that bad inputs are rejected, and checks that the inspector
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

# A freshly started Anvil chain may not have the blocks yet (1 block per second). A
# failed `tip` call is retried; output that has no numeric tip fails at once. This loop
# is the first RPC access, so the chain ID is taken from its last output, not from a
# separate, unretried call.
TIP=-1
for ((i = 0; i < 600; i++)); do
  if TIP_OUT=$($BIN tip 2>&1); then
    TIP=$(printf '%s\n' "$TIP_OUT" | awk '/Tip:/ {sub("#", "", $2); print $2}')
    [[ "$TIP" =~ ^[0-9]+$ ]] || { echo "FAIL: no chain tip in: $TIP_OUT"; exit 1; }
    [ "$TIP" -ge "$LAST" ] && break
  fi
  [ "$i" -eq 0 ] && echo "== waiting for block #$LAST (tip #$TIP)"
  sleep 1
done
[ "$TIP" -ge "$LAST" ] || { echo "FAIL: chain tip #$TIP never reached #$LAST"; exit 1; }
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
