//! The inspector's Node tab: what a Bee node holds and spends, read from its API with `GET`
//! only. It covers the wallet, the chequebook, the cheques and settlements with peers, and
//! the postage batches with their cost per day.
//!
//! Nothing here sends a transaction. A deposit, a withdrawal or a batch top-up is the owner's
//! action (`.claude/agent/postage.md`, rule 3); for a top-up the page prints the command.

use std::collections::BTreeMap;

use alloy::hex;
use anyhow::Result;
use serde_json::{Value, json};

use crate::swarm::bee::{BEE_FACTORY_CHAIN, BatchInfo, Bee, big_number};
use crate::swarm::postage::{GNOSIS_BLOCK_SECS, PLUR_PER_BZZ, amount_for, cost_plur};

/// The endpoints read for the tab, each once.
const PATHS: &[&str] = &[
    "health",
    "node",
    "status",
    "wallet",
    "chequebook/address",
    "chequebook/balance",
    "chequebook/cheque",
    "settlements",
    "timesettlements",
    "chainstate",
    "stamps",
];

/// Peers whose last received cheque is checked for an uncashed amount, at most (one request
/// each).
const CASHOUT_PEERS: usize = 32;
/// Requests to the node at once, as the inspector's other views (`CONCURRENCY`).
const PARALLEL: usize = crate::addr::lookup::CONCURRENCY;
/// How long one endpoint may take: one that hangs (a node waiting on its chain RPC) is then
/// reported as unavailable instead of holding the whole tab.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
/// Peers listed, most settled first.
const PEER_ROWS: usize = 50;
/// A batch with less left than this is flagged.
pub const WARN_TTL_DAYS: f64 = 7.0;
/// A batch whose fullest bucket is at least this full is flagged.
pub const WARN_FILL: f64 = 0.8;
/// Less gas token than this (0.01) is flagged: it pays for deposits, cashouts and top-ups.
const WARN_XDAI_WEI: u128 = 10_000_000_000_000_000;

/// Decimals of xBZZ (PLUR) and of the gas token (wei, on every chain listed in [`chain`]).
const BZZ_DECIMALS: u32 = 16;
const _: () = assert!(10u128.pow(BZZ_DECIMALS) == PLUR_PER_BZZ);
const GAS_DECIMALS: u32 = 18;

/// One endpoint's answer, or why there is none.
type Answer = std::result::Result<Value, String>;

/// Reads every endpoint of the tab from `bee` (in parallel) and summarises them. An endpoint
/// that fails is reported under `unavailable` and the rest is still shown: a node without a
/// chequebook, for example, answers its chequebook endpoints with an error.
pub async fn read(bee: &Bee) -> Result<Value> {
    read_within(bee, DEADLINE).await
}

async fn read_within(bee: &Bee, deadline: std::time::Duration) -> Result<Value> {
    let parts = fetch_all(bee, PATHS.iter().map(|p| p.to_string()), deadline).await;
    // A cheque received from a peer pays only once cashed: ask what is still uncashed.
    let peers: Vec<String> = parts
        .get("chequebook/cheque")
        .and_then(|a| a.as_ref().ok())
        .map(received_cheque_peers)
        .unwrap_or_default();
    let more = peers.len().saturating_sub(CASHOUT_PEERS);
    let cashouts = fetch_all(
        bee,
        peers
            .iter()
            .take(CASHOUT_PEERS)
            .map(|p| format!("chequebook/cashout/{p}")),
        deadline,
    )
    .await;
    let mut out = summarize(&parts, &cashouts);
    out["chequebook"]["cashoutPeersNotChecked"] = json!(more);
    out["url"] = json!(bee.base_url().as_str());
    Ok(out)
}

/// `GET` every path from `bee`, [`PARALLEL`] at a time, each within `deadline`. Every path
/// gets an entry: one whose task panics stays "reading it failed". The tasks live in a
/// `JoinSet`, so a request that is dropped (the page closed) aborts them.
async fn fetch_all(
    bee: &Bee,
    paths: impl Iterator<Item = String>,
    deadline: std::time::Duration,
) -> BTreeMap<String, Answer> {
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(PARALLEL));
    let mut out = BTreeMap::new();
    let mut set = tokio::task::JoinSet::new();
    for path in paths {
        out.insert(path.clone(), Err("reading it failed".to_string()));
        let (bee, permits) = (bee.clone(), permits.clone());
        set.spawn(async move {
            let _permit = permits.acquire_owned().await;
            let a = match tokio::time::timeout(deadline, bee.get_value(&path)).await {
                Ok(a) => a.map_err(|e| format!("{e:#}")),
                Err(_) => Err(format!("no answer within {deadline:?}")),
            };
            (path, a)
        });
    }
    while let Some(r) = set.join_next().await {
        if let Ok((path, a)) = r {
            out.insert(path, a);
        }
    }
    out
}

/// The overlay addresses (64 hex digits) of the peers `GET /chequebook/cheque` lists a
/// received cheque for, the largest cheque first (only the first [`CASHOUT_PEERS`] are asked).
/// Anything else is skipped: the address goes into a request path.
fn received_cheque_peers(cheques: &Value) -> Vec<String> {
    let mut peers: Vec<(u128, &str)> = cheques["lastcheques"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| !c["lastreceived"].is_null())
        .filter_map(|c| {
            Some((
                big(&c["lastreceived"]["payout"]).unwrap_or(0),
                c["peer"].as_str()?,
            ))
        })
        .filter(|(_, p)| p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect();
    peers.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    peers.into_iter().map(|(_, p)| p.to_string()).collect()
}

/// A number Bee sends as a decimal string or a JSON number.
fn big(v: &Value) -> Option<u128> {
    big_number(v, "").ok()
}

/// `v` base units with `decimals` decimals, shown with 4 of them: a balance rounded down (never
/// more than there is), a cost rounded up (never short, as `postage::format_bzz`).
fn units(v: u128, decimals: u32, round_up: bool) -> String {
    let step = 10u128.pow(decimals - 4);
    let v = if round_up { v.div_ceil(step) } else { v / step };
    format!("{}.{:04}", v / 10_000, v % 10_000)
}

/// An amount as its exact base units (a decimal string) and as text.
fn amount(v: u128, decimals: u32, unit: &str, round_up: bool) -> Value {
    json!({ "raw": v.to_string(), "text": format!("{} {unit}", units(v, decimals, round_up)) })
}

/// A balance in xBZZ (rounded down).
fn bzz(v: u128) -> Value {
    amount(v, BZZ_DECIMALS, "xBZZ", false)
}

/// A cost in xBZZ (rounded up).
fn bzz_cost(v: u128) -> Value {
    amount(v, BZZ_DECIMALS, "xBZZ", true)
}

/// The chain a node buys batches on, by the `chainID` of `GET /wallet`: its name, its block
/// time in seconds (`None` where not known) and the token its gas is paid in.
fn chain(id: u64) -> (&'static str, Option<u64>, &'static str) {
    match id {
        100 => (
            "Gnosis Chain (Swarm mainnet)",
            Some(GNOSIS_BLOCK_SECS),
            "xDAI",
        ),
        11_155_111 => ("Sepolia (Swarm testnet)", Some(12), "SepoliaETH"),
        BEE_FACTORY_CHAIN => ("bee-factory dev chain", None, "ETH"),
        _ => ("unknown chain", None, "native token"),
    }
}

/// The tab's document from the endpoints' answers (`parts`, by path) and the cashout status
/// of received cheques (`cashouts`). Pure, so the arithmetic is tested without a node.
fn summarize(parts: &BTreeMap<String, Answer>, cashouts: &BTreeMap<String, Answer>) -> Value {
    let null = Value::Null;
    let get = |p: &str| parts.get(p).and_then(|a| a.as_ref().ok()).unwrap_or(&null);
    let mut warnings: Vec<String> = Vec::new();
    let unavailable: BTreeMap<&str, &str> = parts
        .iter()
        .filter_map(|(p, a)| a.as_ref().err().map(|e| (p.as_str(), e.as_str())))
        .collect();
    // A read that failed must not pass for "nothing to warn about".
    for (path, what) in [
        (
            "stamps",
            "the batches could not be read, so their lifetime and fill were not checked",
        ),
        (
            "chainstate",
            "the storage price could not be read, so no cost per day is shown",
        ),
        (
            "wallet",
            "the wallet could not be read, so its balances and chain are unknown",
        ),
        (
            "chequebook/balance",
            "the chequebook balance could not be read",
        ),
    ] {
        if let Some(e) = unavailable.get(path) {
            warnings.push(format!("GET /{path} failed ({e}): {what}."));
        }
    }

    // The node.
    let (health, node, status, cs) = (get("health"), get("node"), get("status"), get("chainstate"));
    let node_out = json!({
        "version": health["version"],
        "apiVersion": health["apiVersion"],
        "beeMode": node["beeMode"],
        "chequebookEnabled": node["chequebookEnabled"],
        "swapEnabled": node["swapEnabled"],
        "connectedPeers": status["connectedPeers"],
        "isReachable": status["isReachable"],
        "lastSyncedBlock": status["lastSyncedBlock"],
        "chainTip": cs["chainTip"],
    });

    // The wallet.
    let wallet = get("wallet");
    let chain_id = wallet["chainID"].as_u64();
    let (chain_name, block_secs, gas_token) =
        chain_id
            .map(chain)
            .unwrap_or(("not reported", None, "native token"));
    let wallet_bzz = big(&wallet["bzzBalance"]);
    let wallet_gas = big(&wallet["nativeTokenBalance"]);
    if let Some(x) = wallet_gas
        && x < WARN_XDAI_WEI
    {
        warnings.push(format!(
            "The wallet holds {} {gas_token}: little gas for a deposit, a cashout or a top-up.",
            units(x, GAS_DECIMALS, false)
        ));
    }

    // The chequebook. Bee's available balance is the total minus the cheques sent and not
    // cashed yet by the peers.
    let book = get("chequebook/balance");
    let total = big(&book["totalBalance"]);
    let available = big(&book["availableBalance"]);
    let settlements = get("settlements");
    let time = get("timesettlements");
    let mut uncashed_received: u128 = 0;
    let mut cashouts_read = 0;
    let mut cashout_errors = BTreeMap::new();
    for (path, a) in cashouts {
        let peer = path.rsplit('/').next().unwrap_or(path);
        match a {
            Ok(v) => {
                cashouts_read += 1;
                uncashed_received =
                    uncashed_received.saturating_add(big(&v["uncashedAmount"]).unwrap_or(0));
            }
            Err(e) => {
                cashout_errors.insert(peer.to_string(), e.clone());
            }
        }
    }
    if let (Some(t), Some(a)) = (total, available)
        && node["swapEnabled"].as_bool() != Some(false)
        && t > 0
        && a.saturating_mul(10) < t
    {
        warnings.push(format!(
            "The chequebook has {} of its {} left: deposit before it runs dry, or paid traffic \
             (uploads, downloads) stalls.",
            units(a, BZZ_DECIMALS, false),
            units(t, BZZ_DECIMALS, false)
        ));
    }
    let chequebook = json!({
        "address": get("chequebook/address")["chequebookAddress"],
        "total": total.map(bzz),
        "available": available.map(bzz),
        "sentUncashed": total.zip(available).map(|(t, a)| bzz(t.saturating_sub(a))),
        "sent": big(&settlements["totalSent"]).map(bzz),
        "received": big(&settlements["totalReceived"]).map(bzz),
        "timeSent": big(&time["totalSent"]).map(bzz),
        "timeReceived": big(&time["totalReceived"]).map(bzz),
        // Unknown, not zero, when the received cheques could not be listed.
        "receivedUncashed": parts
            .get("chequebook/cheque")
            .is_some_and(|a| a.is_ok())
            .then(|| bzz(uncashed_received)),
        "cashoutPeersChecked": cashouts_read,
        "cashoutErrors": cashout_errors,
    });

    // Peers, most settled first, with the beneficiary of the last cheque sent to each.
    let mut beneficiary: BTreeMap<&str, &Value> = BTreeMap::new();
    for c in get("chequebook/cheque")["lastcheques"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(p) = c["peer"].as_str() {
            beneficiary.insert(p, &c["lastsent"]["beneficiary"]);
        }
    }
    let mut rows: Vec<(u128, u128, &str)> = settlements["settlements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some((
                big(&s["sent"]).unwrap_or(0),
                big(&s["received"]).unwrap_or(0),
                s["peer"].as_str()?,
            ))
        })
        .collect();
    rows.sort_by(|a, b| {
        (b.0.saturating_add(b.1))
            .cmp(&a.0.saturating_add(a.1))
            .then(a.2.cmp(b.2))
    });
    let peers = json!({
        "count": rows.len(),
        "rows": rows.iter().take(PEER_ROWS).map(|(s, r, p)| json!({
            "peer": p,
            "sent": bzz(*s),
            "received": bzz(*r),
            "beneficiary": beneficiary.get(p).copied().unwrap_or(&Value::Null),
        })).collect::<Vec<_>>(),
    });

    // Postage: what each batch costs per day at the current price, and how long it lives.
    let price = big(&cs["currentPrice"]);
    let per_day = price.zip(block_secs).map(|(p, s)| amount_for(p, 86_400, s));
    let mut total_per_day: u128 = 0;
    // A batch Bee describes in a way not understood is named in a warning, never dropped quietly.
    let mut parsed = Vec::new();
    for v in get("stamps")["stamps"].as_array().into_iter().flatten() {
        match serde_json::from_value::<BatchInfo>(v.clone()) {
            Ok(b) => parsed.push((b, v)),
            Err(e) => warnings.push(format!(
                "Batch {} could not be read ({e}): it is missing from the table.",
                v["batchID"].as_str().unwrap_or("without an ID")
            )),
        }
    }
    let batches: Vec<Value> = parsed
        .into_iter()
        .map(|(b, v)| {
            let id = format!("0x{}", hex::encode(b.batch_id));
            let short = &id[..10];
            let cap = b.bucket_capacity();
            let fill = b.utilization as f64 / cap as f64;
            let ttl_days = (b.ttl >= 0).then(|| b.ttl as f64 / 86_400.0);
            let cost = per_day.map(|a| cost_plur(a, b.depth));
            if b.exists && b.usable {
                total_per_day = total_per_day.saturating_add(cost.unwrap_or(0));
            }
            if !b.exists {
                warnings.push(format!("Batch {short}… no longer exists on chain."));
            } else if !b.usable {
                warnings.push(format!("Batch {short}… is not usable (yet, or any more)."));
            }
            match b.immutable {
                Some(true) => {}
                Some(false) => warnings.push(format!(
                    "Batch {short}… is mutable: a full bucket overwrites its oldest chunk, which \
                     loses write-once data without a word. The importer refuses it."
                )),
                None => warnings.push(format!(
                    "Batch {short}…: mutability unknown (Bee did not say). The importer refuses it."
                )),
            }
            if let Some(d) = ttl_days
                && b.exists
                && d < WARN_TTL_DAYS
            {
                warnings.push(format!("Batch {short}… expires in {d:.1} days."));
            }
            if fill >= WARN_FILL {
                warnings.push(format!(
                    "Batch {short}…'s fullest bucket is {:.0} % full: an immutable batch \
                     refuses the chunk that does not fit.",
                    fill * 100.0
                ));
            }
            json!({
                "id": id,
                "label": b.label,
                "depth": b.depth,
                "bucketDepth": b.bucket_depth,
                "immutable": b.immutable,
                "usable": b.usable,
                "exists": b.exists,
                "fullest": b.utilization,
                "capacity": cap,
                "fill": fill,
                "ttlSecs": b.ttl,
                "amount": v["amount"],
                "blockNumber": v["blockNumber"],
                "costPerDay": cost.map(bzz_cost),
                "walletDays": cost.zip(wallet_bzz).and_then(|(c, w)| (c > 0).then(|| w as f64 / c as f64)),
            })
        })
        .collect();
    let postage = json!({
        "price": price.map(|p| p.to_string()),
        "block": cs["block"],
        "blockSecs": block_secs,
        "perChunkPerDay": per_day.map(|a| a.to_string()),
        "costPerDay": (per_day.is_some() && total_per_day > 0).then(|| bzz_cost(total_per_day)),
        "walletDays": wallet_bzz.and_then(|w| (total_per_day > 0).then(|| w as f64 / total_per_day as f64)),
        "batches": batches,
    });

    json!({
        "node": node_out,
        "chain": { "id": chain_id, "name": chain_name },
        "wallet": {
            "address": wallet["walletAddress"],
            "bzz": wallet_bzz.map(bzz),
            "gas": wallet_gas.map(|x| amount(x, GAS_DECIMALS, gas_token, false)),
            "gasToken": gas_token,
        },
        "chequebook": chequebook,
        "peers": peers,
        "postage": postage,
        "warnings": warnings,
        "unavailable": unavailable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers in the shape Bee 2.8.3 gives them on a Swarm-mainnet light node. The values are
    /// made up: addresses, IDs and balances of no real node.
    fn mainnet() -> BTreeMap<String, Answer> {
        let mut m = BTreeMap::new();
        let mut put = |p: &str, v: Value| {
            m.insert(p.to_string(), Ok(v));
        };
        put(
            "health",
            json!({"status":"ok","version":"2.8.3-8e9bb914","apiVersion":"8.2.1"}),
        );
        put(
            "node",
            json!({"beeMode":"light","chequebookEnabled":true,"swapEnabled":true}),
        );
        put(
            "wallet",
            json!({"bzzBalance":"1800000000000000000","nativeTokenBalance":"1999999999999999999","chainID":100,
            "chequebookContractAddress":"0x2222222222222222222222222222222222222222","walletAddress":"0x1111111111111111111111111111111111111111"}),
        );
        put(
            "chequebook/balance",
            json!({"totalBalance":"200000000000000000","availableBalance":"190000000000000001"}),
        );
        put(
            "settlements",
            json!({"totalReceived":"0","totalSent":"10000000000000000","settlements":[
            {"peer":"aa".repeat(32),"received":"0","sent":"30000000000100"},
            {"peer":"bb".repeat(32),"received":"0","sent":"1000000000100"}]}),
        );
        put(
            "chequebook/cheque",
            json!({"lastcheques":[
            {"peer":"bb".repeat(32),"lastreceived":null,"lastsent":{"beneficiary":"0x3333333333333333333333333333333333333333","payout":"1000000000100"}},
            {"peer":"cc".repeat(32),"lastreceived":{"payout":"5"},"lastsent":null}]}),
        );
        put(
            "chainstate",
            json!({"chainTip":49428613,"block":49428605,"totalAmount":"991861671368","currentPrice":"206855","minimumValidityBlocks":17280}),
        );
        put(
            "stamps",
            json!({"stamps":[{"batchID":"44".repeat(32),
            "utilization":44,"usable":true,"label":"test batch","depth":25,"amount":"33676000000","bucketDepth":16,
            "blockNumber":49422504,"immutableFlag":true,"exists":true,"batchTTL":784057}]}),
        );
        m.insert("status".into(), Err("GET status: 503".into()));
        m
    }
    #[test]
    fn balances_round_down_costs_up() {
        assert_eq!(units(1_801_073_693_386_114_678, 16, false), "180.1073");
        assert_eq!(units(1_801_073_693_386_114_678, 16, true), "180.1074");
        assert_eq!(units(0, 18, false), "0.0000");
        assert_eq!(units(99_999, 16, false), "0.0000");
        assert_eq!(units(99_999, 16, true), "0.0001");
        // Costs agree with `ethswarm stamps plan`.
        let c = 3_574_454_400u128 << 25;
        assert_eq!(
            format!("{} BZZ", units(c, 16, true)),
            crate::swarm::postage::format_bzz(c)
        );
    }

    #[test]
    fn mainnet_summary() {
        let mut cashouts = BTreeMap::new();
        cashouts.insert(
            format!("chequebook/cashout/{}", "cc".repeat(32)),
            Ok(json!({"uncashedAmount":"5000000000000000"})),
        );
        let s = summarize(&mainnet(), &cashouts);
        assert_eq!(s["chain"]["name"], "Gnosis Chain (Swarm mainnet)");
        assert_eq!(s["wallet"]["bzz"]["text"], "180.0000 xBZZ");
        assert_eq!(s["wallet"]["gas"]["text"], "1.9999 xDAI");
        // Cheques sent and not cashed: total − available.
        assert_eq!(s["chequebook"]["sentUncashed"]["raw"], "9999999999999999");
        assert_eq!(s["chequebook"]["receivedUncashed"]["text"], "0.5000 xBZZ");
        assert_eq!(s["chequebook"]["cashoutPeersChecked"], 1);
        // Most settled peer first; the beneficiary comes from the last cheque.
        assert_eq!(s["peers"]["rows"][0]["peer"], "aa".repeat(32));
        assert_eq!(
            s["peers"]["rows"][1]["beneficiary"],
            "0x3333333333333333333333333333333333333333"
        );
        // 206,855 PLUR per block × 17,280 blocks a day, × 2^25 chunks.
        let b = &s["postage"]["batches"][0];
        assert_eq!(s["postage"]["perChunkPerDay"], "3574454400");
        assert_eq!(
            b["costPerDay"]["raw"],
            (3_574_454_400u128 << 25).to_string()
        );
        // Rounded up, like every quoted cost.
        assert_eq!(b["costPerDay"]["text"], "11.9939 xBZZ");
        assert_eq!(b["capacity"], 512);
        let days = b["walletDays"].as_f64().unwrap();
        assert!((days - 15.0077).abs() < 1e-3, "{days}");
        // 9.07 days left, immutable, nearly empty: nothing to warn about.
        assert!(
            s["warnings"].as_array().unwrap().is_empty(),
            "{}",
            s["warnings"]
        );
        assert_eq!(s["unavailable"]["status"], "GET status: 503");
    }

    #[test]
    fn warnings() {
        let mut p = mainnet();
        p.insert(
            "stamps".into(),
            Ok(json!({"stamps":[{"batchID":"11".repeat(32),"utilization":420,"usable":true,
                "depth":25,"bucketDepth":16,"immutableFlag":false,"exists":true,"batchTTL":86400}]})),
        );
        p.insert(
            "chequebook/balance".into(),
            Ok(json!({"totalBalance":"1000","availableBalance":"99"})),
        );
        p.insert(
            "wallet".into(),
            Ok(json!({"bzzBalance":"0","nativeTokenBalance":"1","chainID":100})),
        );
        let s = summarize(&p, &BTreeMap::new());
        let w = s["warnings"].to_string();
        for needle in [
            "is mutable",
            "expires in 1.0 days",
            "82 % full",
            "chequebook has",
            "xDAI",
        ] {
            assert!(w.contains(needle), "{needle}: {w}");
        }
    }

    #[test]
    fn cashouts_are_asked_for_hex_peers_largest_cheque_first() {
        let v = json!({"lastcheques":[
            {"peer":"cc".repeat(32),"lastreceived":{"payout":"5"}},
            {"peer":"../wallet","lastreceived":{"payout":"9"}},
            {"peer":"ee".repeat(32),"lastreceived":{"payout":"700"}},
            {"peer":"dd".repeat(32),"lastreceived":null}]});
        assert_eq!(
            received_cheque_peers(&v),
            vec!["ee".repeat(32), "cc".repeat(32)]
        );
    }

    #[test]
    fn an_unreadable_batch_is_named() {
        let mut p = mainnet();
        p.insert(
            "stamps".into(),
            Ok(json!({"stamps":[{"batchID":"12".repeat(32),"usable":true,"batchTTL":"soon"}]})),
        );
        let s = summarize(&p, &BTreeMap::new());
        assert!(s["postage"]["batches"].as_array().unwrap().is_empty());
        let w = s["warnings"].to_string();
        assert!(
            w.contains(&"12".repeat(32)) && w.contains("missing from the table"),
            "{w}"
        );
    }

    #[test]
    fn a_failed_read_is_not_a_negative_answer() {
        let mut p = mainnet();
        p.insert("stamps".into(), Err("GET stamps: timed out".into()));
        p.insert("chequebook/cheque".into(), Err("GET cheque: 500".into()));
        let s = summarize(&p, &BTreeMap::new());
        let w = s["warnings"].to_string();
        assert!(
            w.contains("GET /stamps failed (GET stamps: timed out)"),
            "{w}"
        );
        assert!(s["chequebook"]["receivedUncashed"].is_null());
    }

    #[test]
    fn unknown_mutability_is_said_so() {
        let mut p = mainnet();
        p.insert(
            "stamps".into(),
            Ok(
                json!({"stamps":[{"batchID":"11".repeat(32),"usable":true,"depth":20,
                "bucketDepth":16,"exists":true,"batchTTL":9999999}]}),
            ),
        );
        let w = summarize(&p, &BTreeMap::new())["warnings"].to_string();
        assert!(w.contains("mutability unknown"), "{w}");
    }

    /// `read` against a stand-in node: one endpoint refuses, one hangs past the deadline, and
    /// 40 peers sent cheques, of which only the 32 largest are asked about.
    #[tokio::test]
    async fn read_reports_failures_and_hangs_and_caps_cashouts() {
        use axum::{Json, Router, http::StatusCode, routing::get};
        let cheques = json!({"lastcheques": (0..40u32).map(|i| json!({
            "peer": format!("{i:064x}"), "lastreceived": {"payout": i.to_string()}})).collect::<Vec<_>>()});
        let app = Router::new()
            .route(
                "/wallet",
                get(|| async {
                    Json(json!({"chainID":100,"bzzBalance":"0","nativeTokenBalance":"0"}))
                }),
            )
            .route("/stamps", get(|| async { (StatusCode::BAD_REQUEST, "no") }))
            .route(
                "/chainstate",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    "late"
                }),
            )
            .route(
                "/chequebook/cheque",
                get(move || async move { Json(cheques) }),
            )
            .route(
                "/chequebook/cashout/{peer}",
                get(|| async { Json(json!({"uncashedAmount":"1"})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let bee = Bee::new(format!("http://{addr}/").parse().unwrap())
            .unwrap()
            .with_retries(1);
        let s = read_within(&bee, std::time::Duration::from_millis(500))
            .await
            .unwrap();
        let un = &s["unavailable"];
        assert!(
            un["chainstate"]
                .as_str()
                .unwrap()
                .contains("no answer within"),
            "{un}"
        );
        assert!(un["stamps"].is_string(), "{un}");
        assert!(s["warnings"].to_string().contains("GET /stamps failed"));
        assert_eq!(s["chequebook"]["cashoutPeersChecked"], 32);
        assert_eq!(s["chequebook"]["cashoutPeersNotChecked"], 8);
        assert_eq!(s["chequebook"]["receivedUncashed"]["raw"], "32");
    }

    #[test]
    fn unknown_block_time_gives_no_cost() {
        let mut p = mainnet();
        p.insert(
            "wallet".into(),
            Ok(json!({"bzzBalance":"0","nativeTokenBalance":"0","chainID":1337})),
        );
        let s = summarize(&p, &BTreeMap::new());
        assert!(s["postage"]["perChunkPerDay"].is_null());
        assert!(s["postage"]["batches"][0]["costPerDay"].is_null());
    }
}
