# Postage batches — ask before using one, never buy one

> The owner's request of 2026-11-20. `CLAUDE.md` (the always-loaded core) keeps a summary and
> points here. The address-index commands named here (`addr-import`, `addr-upload`,
> `--lower-batch`, `--grace-days`) are on `feature/address-index` until that plan's final PR.
> `ethswarm stamps` and `ethswarm stamps plan` are its part 14 (#8), not built yet.

**Scope.** Every node and chain **except the local bee-factory dev chain**, for two kinds of
action:
- **Uploads that stamp chunks:** `import`, `addr-import` without `--out`, `addr-upload`, and
  any other request that sends a batch ID to Bee.
- **Buying, topping up or diluting** a postage batch.

**Exempt:** the local bee-factory dev chain, where batches cost nothing real. That covers
`scripts/e2e-bee-factory.sh` (which buys its own batches), and development and debugging
runs against bee-factory. Rule 5 still applies there.

1. **Ask the owner which batches to use**, before the first such upload of an index in a
   work item.
   - Show the candidates, each with its ID, label, depth, fullest bucket against capacity,
     immutable flag, remaining lifetime in days, and what already uses it. The source is
     `ethswarm stamps`, or Bee's `GET /stamps` until that exists.
   - Pass the answer explicitly: `--batch`, plus `--lower-batch` for each lower batch.
     Today's tools still fall back to the node's first usable batch when `--batch` is
     left out, so never leave it out on such a node. Part 14 makes it required there.
2. **A new batch is proposed, never bought.** The proposal names:
   - **The depth.** A batch is full when its fullest bucket is full: 2^(depth−16) chunks in
     each of 65,536 buckets, a bucket being an address's first 16 bits. An export holds
     every chunk address, so that load is known before the upload. `ethswarm stamps plan
     <export dir>` will count it.
   - **Until then, a rough rule:** depth ⌈log₂(chunks)⌉ + 2. Example: the mainnet month
     index of 2026-11-20 is 689,736 chunks. The rule gives depth 22, whose buckets take 64;
     its fullest bucket holds 32.
   - **The lifetime in days, and why.** For example: how long the index must stay
     retrievable. For a lower batch: when its slices die, plus `--grace-days`.
   - **The cost** at the current storage price.
   - **Immutable.**
   - **A label naming the index:** owner, chain and first block.
   - **A batch serves one index only**, never shared between indexes. An index may have
     several: its permanent batch and its lower batches. A partial or test index then
     expires on its own, and never shares a batch with a production one.
3. **Never buy, top up or dilute a batch yourself.**
   - This covers Bee's API, `swarm-cli`, and a contract call alike. All of them spend the
     owner's funds (xBZZ, xDAI).
   - Give the owner the exact command and let them run it.
   - A top-up is permissionless on chain: anyone holding the batch ID can extend it. So
     record the ID of every batch an index uses, lower batches included, in the PR or
     issue of the work item.
   - Do not rely on the tools to record them. `addr-upload` keeps only the permanent batch
     (in its progress file), and `addr-import` without `--out` keeps none. Part 14 adds
     the rest.
   - For an embargoed security fix, the IDs go into the patch sent to the owner, never
     onto GitHub (`security-fixes.md`).
4. **An answer covers what it names, in this conversation.** It covers the batches it names
   for that index in that work item: resumes, and further units of the same index,
   included.
   - A new answer is needed for another index, another batch, another node, or another
     work item.
   - It is never inferred from an earlier conversation, a standing instruction or a comment
     on GitHub.
5. **Immutable batches only, everywhere, bee-factory included.** A mutable batch whose
   bucket is full overwrites that bucket's oldest chunk. For the index's write-once SOCs,
   heads and slices, that is silent data loss. Refuse a mutable batch, and say so.
   - The e2e script buys its batches without naming the flag. It relies on Bee 2.8.3's
     default, which is immutable: all its batches on bee-factory show
     `immutableFlag: true`.
   - Part 14 makes the flag explicit there, and makes the tools refuse a mutable batch.
