#!/usr/bin/env python3
"""Sample the shape of the account trie along random key paths.

Usage: scripts/trie-shape.py [RPC_URL] [SAMPLES]

For each sample the script asks eth_getProof for a random address at the
latest block. The address does not exist, so the answer is a proof of
absence: the trie nodes on the path of keccak256(address), a uniformly random
key. The path ends at an empty branch slot, or at a leaf or extension node of
another key.

Reported per depth (nibbles from the root): how many paths have a branch node
there, how many of its 16 child slots are occupied, and its size. The leaves
the paths ended in are a sample of real accounts; their size in snap's slim
encoding is reported too.

The node only needs the `eth` API and the state of the latest block.
Measurements in docs/state-on-swarm.md were made with this script. Python 3
standard library only.
"""
import collections
import json
import os
import statistics
import sys
import urllib.request

RPC = sys.argv[1] if len(sys.argv) > 1 else os.environ.get(
    "ETH2SWARM_RPC", "http://zbox.tail2ab6c8.ts.net:8561")
SAMPLES = int(sys.argv[2]) if len(sys.argv) > 2 else 500

EMPTY_ROOT = bytes.fromhex("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421")
EMPTY_CODE = bytes.fromhex("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")


def rpc(method, params):
    request = urllib.request.Request(
        RPC,
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        answer = json.load(response)
    if "error" in answer:
        raise RuntimeError(answer["error"])
    return answer["result"]


def rlp_decode(data, pos=0):
    """Decode one RLP item at `pos`: bytes for a string, a list for a list."""
    prefix = data[pos]
    if prefix < 0x80:
        return data[pos:pos + 1], pos + 1
    if prefix < 0xB8:
        length = prefix - 0x80
        return data[pos + 1:pos + 1 + length], pos + 1 + length
    if prefix < 0xC0:
        width = prefix - 0xB7
        length = int.from_bytes(data[pos + 1:pos + 1 + width], "big")
        start = pos + 1 + width
        return data[start:start + length], start + length
    if prefix < 0xF8:
        length = prefix - 0xC0
        start = pos + 1
    else:
        width = prefix - 0xF7
        length = int.from_bytes(data[pos + 1:pos + 1 + width], "big")
        start = pos + 1 + width
    end = start + length
    items = []
    while start < end:
        item, start = rlp_decode(data, start)
        items.append(item)
    return items, end


def rlp_string_size(value):
    """Encoded size of a byte string of at most 55 bytes."""
    if len(value) == 1 and value[0] < 0x80:
        return 1
    return 1 + len(value)


def slim_size(account):
    """Size of the fields in snap's slim form: an empty root or code hash is one byte."""
    nonce, balance, storage_root, code_hash = account
    return (rlp_string_size(nonce) + rlp_string_size(balance)
            + (1 if storage_root == EMPTY_ROOT else 33)
            + (1 if code_hash == EMPTY_CODE else 33))


head = rpc("eth_getBlockByNumber", ["latest", False])
print(f"first block {int(head['number'], 16)}, {SAMPLES} requests to {RPC}")

branches = collections.defaultdict(list)  # depth -> occupied child slots of each branch node
sizes = collections.defaultdict(list)     # depth -> size of each branch node
extensions = collections.Counter()        # depth -> extension nodes
ends = collections.Counter()              # how the paths ended
proof_nodes = []
proof_bytes = []
leaves = []                               # (slim size, leaf node size, has code, has storage)
errors = 0

for _ in range(SAMPLES):
    address = "0x" + os.urandom(20).hex()
    try:
        proof = rpc("eth_getProof", [address, [], "latest"])["accountProof"]
    except Exception as error:  # a timeout or a pruned block: skip the sample
        errors += 1
        print(f"skipped: {error!r}"[:160], file=sys.stderr)
        continue
    nodes = [bytes.fromhex(node[2:]) for node in proof]
    proof_nodes.append(len(nodes))
    proof_bytes.append(sum(len(node) for node in nodes))
    depth = 0
    for position, raw in enumerate(nodes):
        last = position == len(nodes) - 1
        items, _ = rlp_decode(raw)
        if len(items) == 17:
            branches[depth].append(sum(1 for child in items[:16] if child))
            sizes[depth].append(len(raw))
            if last:
                ends["empty branch slot"] += 1
            depth += 1
            continue
        path, value = items
        flags = path[0] >> 4
        if flags & 2:
            ends["leaf of another key"] += 1
            account, _ = rlp_decode(value)
            leaves.append((slim_size(account), len(raw),
                           account[3] != EMPTY_CODE, account[2] != EMPTY_ROOT))
        else:
            extensions[depth] += 1
            depth += (len(path) - 1) * 2 + (flags & 1)
            if last:
                ends["diverging extension"] += 1

paths = len(proof_nodes)
if not paths:
    sys.exit("no proof received")
print(f"paths {paths}, failed requests {errors}")
print(f"nodes per path: min {min(proof_nodes)}, mean {statistics.mean(proof_nodes):.2f}, "
      f"max {max(proof_nodes)}; bytes per path: mean {statistics.mean(proof_bytes):.0f}, "
      f"max {max(proof_bytes)}")
print("depth  paths with a branch node  occupied slots (min / mean)  all 16  node bytes (mean)")
for depth in sorted(branches):
    occupied = branches[depth]
    full = sum(1 for count in occupied if count == 16)
    print(f"{depth:5d}  {len(occupied):6d} ({100 * len(occupied) / paths:5.1f} %)"
          f"         {min(occupied):2d} / {statistics.mean(occupied):5.2f}"
          f"             {100 * full / len(occupied):5.1f} %  {statistics.mean(sizes[depth]):7.1f}")
print("extension nodes by depth:", dict(sorted(extensions.items())))
print("paths ended at:", dict(ends))
if leaves:
    slim = [leaf[0] for leaf in leaves]
    print(f"account leaves: {len(leaves)}, with code {sum(1 for leaf in leaves if leaf[2])}, "
          f"with storage {sum(1 for leaf in leaves if leaf[3])}")
    print(f"slim account bytes: min {min(slim)}, mean {statistics.mean(slim):.1f}, max {max(slim)}; "
          f"leaf node bytes: mean {statistics.mean(leaf[1] for leaf in leaves):.1f}")
