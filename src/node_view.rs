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

use crate::swarm::bee::{BatchInfo, Bee};
use crate::swarm::postage::{GNOSIS_BLOCK_SECS, amount_for, cost_plur};

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
/// Peers listed, most settled first.
const PEER_ROWS: usize = 50;
/// A batch with less left than this is flagged.
pub const WARN_TTL_DAYS: f64 = 7.0;
/// A batch whose fullest bucket is at least this full is flagged.
pub const WARN_FILL: f64 = 0.8;
/// Less xDAI than this (0.01) is flagged: it pays the gas of deposits, cashouts and top-ups.
const WARN_XDAI_WEI: u128 = 10_000_000_000_000_000;

/// Decimals of xBZZ (PLUR) and of xDAI (wei).
const BZZ_DECIMALS: u32 = 16;
const XDAI_DECIMALS: u32 = 18;

/// One endpoint's answer, or why there is none.
type Answer = std::result::Result<Value, String>;

/// Reads every endpoint of the tab from `bee` (in parallel) and summarises them. An endpoint
/// that fails is reported under `unavailable` and the rest is still shown: a node without a
/// chequebook, for example, answers its chequebook endpoints with an error.
pub async fn read(bee: &Bee) -> Result<Value> {
    let parts = fetch_all(bee, PATHS.iter().map(|p| p.to_string())).await;
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
    )
    .await;
    let mut out = summarize(&parts, &cashouts);
    out["chequebook"]["cashoutPeersNotChecked"] = json!(more);
    out["url"] = json!(bee.base_url().as_str());
    Ok(out)
}

async fn fetch_all(bee: &Bee, paths: impl Iterator<Item = String>) -> BTreeMap<String, Answer> {
    let mut set = tokio::task::JoinSet::new();
    for path in paths {
        let bee = bee.clone();
        set.spawn(async move {
            let a = bee.get_value(&path).await.map_err(|e| format!("{e:#}"));
            (path, a)
        });
    }
    let mut out = BTreeMap::new();
    while let Some(r) = set.join_next().await {
        if let Ok((path, a)) = r {
            out.insert(path, a);
        }
    }
    out
}

/// The overlay addresses (64 hex digits) of the peers `GET /chequebook/cheque` lists a
/// received cheque for. Anything else is skipped: the address goes into a request path.
fn received_cheque_peers(cheques: &Value) -> Vec<String> {
    cheques["lastcheques"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| !c["lastreceived"].is_null())
        .filter_map(|c| c["peer"].as_str())
        .filter(|p| p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
        .collect()
}

/// A number Bee sends as a decimal string or a JSON number.
fn big(v: &Value) -> Option<u128> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(u128::from),
        _ => None,
    }
}

/// `v` base units with `decimals` decimals, shown with `places` of them, rounded down.
fn units(v: u128, decimals: u32, places: u32) -> String {
    let one = 10u128.pow(decimals);
    let frac = (v % one) / 10u128.pow(decimals - places);
    format!("{}.{:0width$}", v / one, frac, width = places as usize)
}

/// An amount as its exact base units (a decimal string) and as text.
fn amount(v: u128, decimals: u32, unit: &str) -> Value {
    json!({ "raw": v.to_string(), "text": format!("{} {unit}", units(v, decimals, 4)) })
}

fn bzz(v: u128) -> Value {
    amount(v, BZZ_DECIMALS, "xBZZ")
}

/// The chain a node buys batches on, by the `chainID` of `GET /wallet`, and its block time in
/// seconds (`None` where not known).
fn chain(id: u64) -> (&'static str, Option<u64>) {
    match id {
        100 => ("Gnosis Chain (Swarm mainnet)", Some(GNOSIS_BLOCK_SECS)),
        11_155_111 => ("Sepolia (Swarm testnet)", Some(12)),
        1337 => ("bee-factory dev chain", None),
        _ => ("unknown chain", None),
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
    let (chain_name, block_secs) = chain_id.map(chain).unwrap_or(("not reported", None));
    let wallet_bzz = big(&wallet["bzzBalance"]);
    let wallet_xdai = big(&wallet["nativeTokenBalance"]);
    if let Some(x) = wallet_xdai
        && x < WARN_XDAI_WEI
    {
        warnings.push(format!(
            "The wallet holds {} xDAI: little gas for a deposit, a cashout or a top-up.",
            units(x, XDAI_DECIMALS, 4)
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
    let mut cashout_errors = BTreeMap::new();
    for (path, a) in cashouts {
        let peer = path.rsplit('/').next().unwrap_or(path);
        match a {
            Ok(v) => uncashed_received += big(&v["uncashedAmount"]).unwrap_or(0),
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
            units(a, BZZ_DECIMALS, 4),
            units(t, BZZ_DECIMALS, 4)
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
        "receivedUncashed": bzz(uncashed_received),
        "cashoutPeersChecked": cashouts.len(),
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
    rows.sort_by(|a, b| (b.0 + b.1).cmp(&(a.0 + a.1)).then(a.2.cmp(b.2)));
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
    let batches: Vec<Value> = get("stamps")["stamps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| serde_json::from_value::<BatchInfo>(v.clone()).ok().map(|b| (b, v)))
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
            if b.immutable != Some(true) {
                warnings.push(format!(
                    "Batch {short}… is not immutable: a full bucket overwrites its oldest chunk, \
                     which loses write-once data without a word."
                ));
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
                "costPerDay": cost.map(bzz),
                "walletDays": cost.zip(wallet_bzz).and_then(|(c, w)| (c > 0).then(|| w as f64 / c as f64)),
            })
        })
        .collect();
    let postage = json!({
        "price": price.map(|p| p.to_string()),
        "block": cs["block"],
        "blockSecs": block_secs,
        "perChunkPerDay": per_day.map(|a| a.to_string()),
        "costPerDay": (per_day.is_some() && total_per_day > 0).then(|| bzz(total_per_day)),
        "walletDays": wallet_bzz.and_then(|w| (total_per_day > 0).then(|| w as f64 / total_per_day as f64)),
        "batches": batches,
    });

    json!({
        "node": node_out,
        "chain": { "id": chain_id, "name": chain_name },
        "wallet": {
            "address": wallet["walletAddress"],
            "bzz": wallet_bzz.map(bzz),
            "xdai": wallet_xdai.map(|x| amount(x, XDAI_DECIMALS, "xDAI")),
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

    /// Answers as Bee 2.8.3 gave them on the Swarm-mainnet light node, 2026-11-23.
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
            json!({"bzzBalance":"1801073693386114678","nativeTokenBalance":"1999999999894191124","chainID":100,
            "chequebookContractAddress":"0x614f90324e7ac11a08c29f3313de9f4c7e047b77","walletAddress":"0x0b522dfb53807fd9a9a974ee19d2637205970086"}),
        );
        put(
            "chequebook/balance",
            json!({"totalBalance":"212000000000000000","availableBalance":"194592582999977000"}),
        );
        put(
            "settlements",
            json!({"totalReceived":"0","totalSent":"17410018000023000","settlements":[
            {"peer":"aa".repeat(32),"received":"0","sent":"32703000000100"},
            {"peer":"bb".repeat(32),"received":"0","sent":"1198000000100"}]}),
        );
        put(
            "chequebook/cheque",
            json!({"lastcheques":[
            {"peer":"bb".repeat(32),"lastreceived":null,"lastsent":{"beneficiary":"0x621447dBaC6b0f195179dCBD765D1835F5229B34","payout":"1198000000100"}},
            {"peer":"cc".repeat(32),"lastreceived":{"payout":"5"},"lastsent":null}]}),
        );
        put(
            "chainstate",
            json!({"chainTip":49428613,"block":49428605,"totalAmount":"991861671368","currentPrice":"206855","minimumValidityBlocks":17280}),
        );
        put(
            "stamps",
            json!({"stamps":[{"batchID":"da4c9e51e6c8bb6085aa773320879ffa32ec78da82f5cf5a02fe5d92aca4b932",
            "utilization":44,"usable":true,"label":"addr1 year","depth":25,"amount":"33676000000","bucketDepth":16,
            "blockNumber":49422504,"immutableFlag":true,"exists":true,"batchTTL":784057}]}),
        );
        m.insert("status".into(), Err("GET status: 503".into()));
        m
    }

    #[test]
    fn units_round_down() {
        assert_eq!(units(1_801_073_693_386_114_678, 16, 4), "180.1073");
        assert_eq!(units(0, 18, 4), "0.0000");
        assert_eq!(units(99_999, 16, 4), "0.0000");
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
        assert_eq!(s["wallet"]["bzz"]["text"], "180.1073 xBZZ");
        assert_eq!(s["wallet"]["xdai"]["text"], "1.9999 xDAI");
        // Cheques sent and not cashed: total − available.
        assert_eq!(s["chequebook"]["sentUncashed"]["raw"], "17407417000023000");
        assert_eq!(s["chequebook"]["receivedUncashed"]["text"], "0.5000 xBZZ");
        // Most settled peer first; the beneficiary comes from the last cheque.
        assert_eq!(s["peers"]["rows"][0]["peer"], "aa".repeat(32));
        assert_eq!(
            s["peers"]["rows"][1]["beneficiary"],
            "0x621447dBaC6b0f195179dCBD765D1835F5229B34"
        );
        // 206,855 PLUR per block × 17,280 blocks a day, × 2^25 chunks.
        let b = &s["postage"]["batches"][0];
        assert_eq!(s["postage"]["perChunkPerDay"], "3574454400");
        assert_eq!(
            b["costPerDay"]["raw"],
            (3_574_454_400u128 << 25).to_string()
        );
        assert_eq!(b["costPerDay"]["text"], "11.9938 xBZZ");
        assert_eq!(b["capacity"], 512);
        let days = b["walletDays"].as_f64().unwrap();
        assert!((days - 15.0168).abs() < 1e-3, "{days}");
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
            "not immutable",
            "expires in 1.0 days",
            "82 % full",
            "chequebook has",
            "xDAI",
        ] {
            assert!(w.contains(needle), "{needle}: {w}");
        }
    }

    #[test]
    fn only_hex_peers_are_asked_for_cashouts() {
        let v = json!({"lastcheques":[
            {"peer":"cc".repeat(32),"lastreceived":{}},
            {"peer":"../wallet","lastreceived":{}},
            {"peer":"dd".repeat(32),"lastreceived":null}]});
        assert_eq!(received_cheque_peers(&v), vec!["cc".repeat(32)]);
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
