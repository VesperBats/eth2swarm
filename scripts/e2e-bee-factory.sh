#!/usr/bin/env bash
# End-to-end test of the SOC block store against a running bee-factory stack.
#
#   bee-factory start --tag v2.8.3
#   scripts/e2e-bee-factory.sh
#
# Reads Ethereum mainnet blocks from the zbox node (override with ETH2SWARM_RPC), imports them
# via worker 1 (which holds the postage batch), then reads and verifies them through the queen
# (proves p2p retrieval), and checks that bad inputs are rejected.
#
# Against the local Anvil chain instead:
#   ETH2SWARM_RPC=http://localhost:8545 scripts/e2e-bee-factory.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# SOC owner key: Anvil dev account #0 — a public Foundry test key, only for the dev stack.
KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
OWNER=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
EPOCH_SIZE=${EPOCH_SIZE:-64}
EPOCHS=${EPOCHS:-4}

export ETH2SWARM_RPC=${ETH2SWARM_RPC:-http://zbox.tail2ab6c8.ts.net:8561}
export RUST_LOG=${RUST_LOG:-warn}
cargo build -q
BIN=target/debug/ethswarm
CHAIN_ID=$($BIN tip | awk '/Chain ID:/ {print $3}')

echo "== import $EPOCHS epochs × $EPOCH_SIZE blocks via worker 1"
OUT=$($BIN --bee http://localhost:1635 import --key "$KEY" --epoch-size "$EPOCH_SIZE" --epochs "$EPOCHS")
echo "$OUT"
ANCHOR=$(echo "$OUT" | awk '/Trust anchor:/ {print $3}')
LAST=$((EPOCH_SIZE * EPOCHS - 1))

client() { $BIN --bee http://localhost:1633 "$@" --owner "$OWNER" --chain-id "$CHAIN_ID" --anchor "$ANCHOR"; }

echo "== verify blocks via queen"
for n in 0 3 $((LAST / 2)) "$LAST"; do client get "$n"; done

echo "== verify every epoch via queen"
for ((k = 0; k < EPOCHS; k++)); do client verify-epoch "$k"; done

expect_fail() {
  local what=$1; shift
  if "$@" >/dev/null 2>&1; then echo "FAIL: $what was accepted"; exit 1; fi
  echo "ok: $what rejected"
}

echo "== negative tests"
BAD=0x$(printf '11%.0s' {1..32})
expect_fail "wrong anchor" $BIN --bee http://localhost:1633 get 3 --owner "$OWNER" \
  --chain-id "$CHAIN_ID" --anchor "$BAD"
expect_fail "wrong owner" $BIN --bee http://localhost:1633 get 3 \
  --owner 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 --chain-id "$CHAIN_ID" --anchor "$ANCHOR"
expect_fail "block outside accumulator" client get $((LAST + 1))

echo "== all checks passed (anchor $ANCHOR)"
