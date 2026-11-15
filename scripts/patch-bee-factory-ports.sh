#!/usr/bin/env bash
# Patch bee-factory to run on non-default ports so it coexists with other
# blockchain containers (e.g. a Gnosis nethermind on :8545 or a DAppNode Bee
# on :1634). Never edits the global npm package in place — it copies the
# package to ~/tools/bee-factory-patched, patches the copy, and installs a
# shim at $(npm prefix -g)/bin/bee-factory.
#
# Port layout after patch:
#   Anvil RPC        8545 -> 8546
#   Bee API/P2P      1633-1642 -> 1733-1742
#
# Re-run after every `npm update -g @ethersphere/bee-factory`.
# Requires: node >= 18, @ethersphere/bee-factory installed globally.
set -euo pipefail

PKG="$(npm root -g)/@ethersphere/bee-factory"
DEST="$HOME/tools/bee-factory-patched"

[ -d "$PKG" ] || { echo "error: $PKG not found — run: npm install -g --allow-scripts=protobufjs,ssh2 @ethersphere/bee-factory" >&2; exit 1; }

rm -rf "$DEST"
mkdir -p "$(dirname "$DEST")"
cp -r "$PKG" "$DEST"

BF="$DEST/dist"

# 1. Anvil host+container port 8545 -> 8546
sed -i 's/exports.ANVIL_PORT = 8545;/exports.ANVIL_PORT = 8546;/' "$BF/config.js"

# 2. Bee node ports 16xx -> 17xx
sed -i -E 's/((api|p2p)Port: )16(.{2})/\117\3/g' "$BF/config.js"

# 3. The package maps the host port but never tells anvil to listen on it
#    (anvil defaults to 8545 inside the container). Add the missing --port flag.
sed -i "s/'--host', '0.0.0.0',/'--host', '0.0.0.0', '--port', String(config_1.ANVIL_PORT),/" "$BF/docker/manager.js"

# 4. Hardcoded :8545 strings (readiness-probe heuristic + startup banner)
sed -i 's/:8545/:8546/g' "$BF/docker/manager.js" "$BF/commands/start.js"

# Verify the patches landed before overwriting the shim.
grep -q 'exports.ANVIL_PORT = 8546;' "$BF/config.js"
grep -q "'--port', String(config_1.ANVIL_PORT)" "$BF/docker/manager.js"
grep -q 'apiPort: 1733' "$BF/config.js"

BIN_DIR="$(npm prefix -g)/bin"
mkdir -p "$BIN_DIR"
cat > "$BIN_DIR/bee-factory" <<'EOF'
#!/bin/sh
# shim -> port-patched bee-factory copy (Anvil 8546, Bee 1733-1742)
exec node "$HOME/tools/bee-factory-patched/dist/cli.js" "$@"
EOF
chmod +x "$BIN_DIR/bee-factory"

echo "patched: Anvil :8546, Bee :1733-1742 (pristine package untouched)"
echo "shim:    $BIN_DIR/bee-factory -> $BF/cli.js"
