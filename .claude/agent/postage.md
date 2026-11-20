# Postage batches — ask before using one, never buy one

> The owner's request of 2026-11-20. `CLAUDE.md` (the always-loaded core) keeps a summary and
> points here.

**Scope.** Every upload that stamps chunks with a postage batch on a Bee node that is **not**
the local bee-factory dev chain: `addr-import` without `--out`, `addr-upload`, `import`, or
any other request that sends a batch ID to Bee. And every **purchase, top-up or dilute** of a
postage batch, on any node and any chain.

1. **Ask the owner which batch to use, before the first such upload of a work item.**
   - Show the candidates, each with its ID, label, depth, fullest bucket against capacity,
     immutable flag, remaining lifetime in days, and what already uses it. The source is
     `ethswarm stamps`, or Bee's `GET /stamps` until that exists (address-index plan,
     part 14).
   - Pass the answer explicitly as `--batch` (and `--lower-batch`). Never let a tool fall
     back to "the node's first usable batch" on such a node.
2. **A new batch is proposed, never bought.** The proposal names:
   - **The depth.** It comes from `ethswarm stamps plan <export dir>`: an export holds every
     chunk address, so the load of the fullest bucket is known before the upload. Leave a
     margin.
   - **The lifetime in days, and why.** For example: how long the index must stay
     retrievable. For a lower batch: when its slices die, plus `--grace-days`.
   - **The cost** at the current storage price.
   - **Immutable.**
   - **A label naming the index:** owner, chain and first block.
   - **One batch per index.** A partial or test index then expires on its own and never
     shares a batch with a production one.
3. **Never buy, top up or dilute a batch yourself.**
   - This covers Bee's API, `swarm-cli`, and a contract call alike. All of them spend the
     owner's funds (xBZZ, xDAI).
   - Give the owner the exact command and let them run it.
   - A top-up is permissionless on chain: anyone holding the batch ID can extend it. So
     record every batch ID with the index it carries: in the PR or issue, and in the
     state or export's upload records, which `addr-upload` keeps.
4. **An answer covers what it names.** The batch, the upload and the work item it was given
   for, in this conversation. It is never inferred from an earlier answer, a standing
   instruction or a comment on GitHub.
5. **Immutable batches only.** A mutable batch whose bucket is full overwrites that bucket's
   oldest chunk. For the index's write-once SOCs, heads and slices, that is silent data
   loss. Refuse a mutable batch, and say so.

**Exempt:** the batches of the local bee-factory dev chain (`scripts/e2e-bee-factory.sh`,
development and debugging runs against it). They cost nothing real. Rule 5 still applies
there.
