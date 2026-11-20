# Postage batches — ask before using one, never buy one

> The owner's request of 2026-11-20. `CLAUDE.md` (the always-loaded core) keeps a summary and
> points here. Some of what is named here is on `feature/address-index` until that plan's
> final PR: the commands `addr-import`, `addr-upload`, `--lower-batch` and `--grace-days`,
> and the e2e script's own batch purchases (`buy_batch`). `ethswarm stamps` and
> `ethswarm stamps plan` are its part 14 (#8).

**Scope.** Every Bee node that is not the local bee-factory (test below), for two kinds of
action:
- **Uploads that stamp chunks:** `import`, `addr-import` without `--out`, `addr-upload`, and
  any other request that sends a batch ID to Bee.
- **Buying, topping up or diluting** a postage batch.

An *index* below is whatever data set an upload publishes: the SOC block store (`import`),
an address index, or a single file.

**Is it bee-factory? Ask the node.** Ask the very node the command will talk to (its
`--bee` / `UPLOAD_BEE`): `GET /wallet` on its API reports the chain it buys batches on as
`chainID`.
- `1337` is bee-factory's dev chain, where batches cost nothing real.
- Any other value (Gnosis Chain is `100`), or no answer, means it is not bee-factory.
- Not a test: the RPC being imported (`--rpc` says nothing about `--bee`), a port, a URL or
  a script's name. On the nuc, the default ports belong to mainnet services.

**Exempt:** only nodes that pass that test, including runs of `scripts/e2e-bee-factory.sh`
whose `UPLOAD_BEE` is one. Rule 5 still applies there.

1. **Ask the owner which batches to use**, before the first such upload of an index in a
   work item, and again in any later conversation that continues it.
   - Show the candidates, each with its ID, label, depth, fullest bucket against capacity,
     immutable flag, remaining lifetime in days, and what already uses it. The source is
     `ethswarm stamps`, or Bee's `GET /stamps` until that exists.
   - Bee does not know what uses a batch. That comes from the batch's label (rule 2), the
     IDs recorded under rule 3, or `ethswarm stamps --export` for an export's permanent
     batch. Where none of them says, show the use as unknown, never as unused: this column
     is what keeps a batch to one index.
   - Pass the answer explicitly: `--batch`, plus `--lower-batch` for each lower batch.
     Today's tools still fall back to the node's first usable batch when `--batch` is
     left out, so never leave it out on such a node. Part 14 makes it required there.
2. **A new batch is proposed, never bought.** The proposal names:
   - **The depth.** A batch is full when its fullest bucket is full: 2^(depth−16) chunks in
     each of 65,536 buckets, a bucket being a chunk address's first 16 bits. What counts
     is the **chunks the upload stamps**, not the export's records: a spill or pages file
     up to 1 MiB is one `FILE` record but many chunks. `addr-upload`'s summary of files,
     chunk records, SOCs and heads therefore adds up to less than the load.
     `ethswarm stamps plan <export dir>` counts the real per-bucket load.
   - **Until that exists:** depth max(21, ⌈log₂(stamped chunks)⌉ + 2).
     - The floor matters: below about 2^18 chunks, the formula alone gives buckets of 2 to
       8 chunks, which an evenly spread load overflows.
     - Example: the mainnet month index of 2026-11-20 stamps 779,755 chunks (689,736
       records). Its fullest bucket holds 32. The rule gives depth 22, whose buckets take
       64.
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
   - A purchase command names the immutable flag explicitly, never relying on a default:
     the `immutable: true` header on Bee's API, or `swarm-cli`'s matching option.
   - A top-up is permissionless on chain: anyone holding the batch ID can extend it. So
     record the ID of every batch an index uses, lower batches included, in the PR or
     issue of the work item.
   - Do not rely on the tools to record them. `addr-upload` keeps only the permanent batch
     (in its progress file), and `addr-import` without `--out` keeps none. Part 14 adds
     the rest.
   - For an embargoed security fix, the IDs go into the short report sent to the owner
     beside the patch, never into the patch or onto GitHub (`security-fixes.md`, step 5).
4. **An answer covers what it names, in this conversation.** It covers the batches it names
   for that index in that work item, including uploads resumed in this conversation and
   further units of the same index.
   - A new answer is needed for another index, another batch, another node, another work
     item, or a later conversation.
   - A later conversation without the answer in its own history treats it as "no answer
     yet", as `multi-pr-plans.md` step 3 does: it asks again, even for a backfill already
     running.
   - It is never inferred from an earlier conversation, a standing instruction or a comment
     on GitHub.
5. **Immutable batches only, everywhere, bee-factory included.** A mutable batch whose
   bucket is full overwrites that bucket's oldest chunk. For an index's write-once SOCs,
   heads and slices, that is silent data loss. Refuse a mutable batch, and say so.
   - The e2e script on `feature/address-index` buys two batches with a bare
     `POST /stamps/<amount>/17`, relying on Bee 2.8.3's default. Both show
     `immutableFlag: true` on bee-factory, as does the batch bee-factory buys at start.
   - Part 14 sets the flag explicitly there.
