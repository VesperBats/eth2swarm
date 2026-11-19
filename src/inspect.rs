//! `ethswarm serve`: a local web UI for inspecting what the Swarm nodes hold.
//!
//! The page (`inspect.html`, embedded in the binary) talks to a small JSON API here, which
//! fetches from a chosen Bee/ant node and decodes and verifies with the same code the CLI
//! client uses — so what the page shows is exactly what `ethswarm get` would accept.

use crate::accumulator::{self, root_from_proof};
use crate::addr::detect::role;
use crate::addr::layout::{
    AddressRecord, BucketBody, BucketRecord, Entries, Entry, HeadRecord, SliceRecord,
};
use crate::addr::lookup::{CONCURRENCY, lookup as addr_lookup, newest_head_by_probing, read_plan};
use crate::addr::slice::{SliceId, bucket_of, fingerprint, held};
use crate::addr::verify::IndexReader;
use crate::chain::{Body, check_body, header_field_count};
use crate::layout::{self, AnchorRecord, EpochRecord, Geometry, HeaderRecord};
use crate::swarm::{bee::Bee, bmt::CHUNK_SIZE, soc};
use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{Header, Transaction, TxEnvelope};
use alloy::eips::Typed2718;
use alloy::hex;
use alloy::primitives::{Address, B256, keccak256};
use alloy::rlp::Decodable;
use alloy::transports::http::reqwest::Url;
use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

const PAGE: &str = include_str!("inspect.html");

/// A named Swarm node the UI can read from.
#[derive(Clone)]
pub struct Node {
    pub name: String,
    pub url: Url,
}

/// Parse `name=url` (or a bare URL, named after its host:port).
pub fn parse_node(s: &str) -> Result<Node, String> {
    let (name, url) = match s.split_once('=') {
        Some((n, u)) => (n.to_string(), u),
        None => (String::new(), s),
    };
    let url: Url = url
        .parse()
        .map_err(|e| format!("bad node URL {url}: {e}"))?;
    let name = if name.is_empty() {
        format!(
            "{}:{}",
            url.host_str().unwrap_or("?"),
            url.port_or_known_default().unwrap_or(0)
        )
    } else {
        name
    };
    Ok(Node { name, url })
}

/// Defaults shown in the UI's settings bar.
pub struct Defaults {
    pub owner: Option<Address>,
    pub chain_id: u64,
    pub anchor: Option<B256>,
}

struct AppState {
    nodes: Vec<Node>,
    defaults: Defaults,
    /// The newest address-index head update found per (node, owner, chain): the next search
    /// starts there instead of at update 1 (heads are write-once, so it still exists).
    heads: std::sync::Mutex<std::collections::HashMap<(String, Address, u64), u64>>,
}

impl AppState {
    fn bee(&self, node: Option<&str>) -> Result<Bee> {
        let node = match node {
            Some(name) => self
                .nodes
                .iter()
                .find(|n| n.name == name)
                .ok_or_else(|| anyhow!("unknown node {name}"))?,
            None => self.nodes.first().context("no nodes configured")?,
        };
        Ok(Bee::new(node.url.clone())?.with_retries(1))
    }
}

/// Serves the inspector on `listen`, answering the `Host` headers [`HostPolicy`] accepts for
/// the address actually bound (with `allow_hosts`, further names or IP addresses).
pub async fn serve(
    listen: SocketAddr,
    allow_hosts: &[String],
    nodes: Vec<Node>,
    defaults: Defaults,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    // The bound address: with port 0 the system picked the port.
    let listen = listener.local_addr()?;
    let hosts = HostPolicy::new(listen, allow_hosts)?;
    let state = Arc::new(AppState {
        nodes,
        defaults,
        heads: Default::default(),
    });
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/config", get(config))
        .route("/api/soc", get(soc_handler))
        .route("/api/block", get(block_handler))
        .route("/api/epoch", get(epoch_handler))
        .route("/api/bytes", get(bytes_handler))
        .route("/api/addr/lookup", get(addr_lookup_handler))
        .route("/api/addr/slice", get(addr_slice_handler))
        .route("/api/addr/bucket", get(addr_bucket_handler))
        .route("/api/addr/spill", get(addr_spill_handler))
        .route("/api/addr/examples", get(addr_examples_handler))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(Arc::new(hosts), check_host));
    if !listen.ip().is_loopback() {
        eprintln!(
            "WARNING: listening on {listen}, which is not loopback. The inspector has no \
             authentication; anyone who can reach it can read the configured nodes through it."
        );
    }
    println!("Swarm inspector on http://{listen}/");
    for h in allow_hosts {
        println!("  accepting Host {h}:{}", listen.port());
    }
    for n in &state.nodes {
        println!("  node {:<10} {}", n.name, n.url);
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// Which `Host` headers the server answers. Requests naming anything else are refused: a web
/// page the browser is on could otherwise reach the inspector through a DNS name it controls
/// that resolves to this machine (DNS rebinding). An IP literal cannot be rebound, so the
/// listen address is accepted, and when the server listens on every address (`0.0.0.0` /
/// `::`) any IP literal; names only when they are loopback names or allowed explicitly.
#[derive(Clone, Debug)]
pub struct HostPolicy {
    port: u16,
    listen_ip: std::net::IpAddr,
    /// Hosts allowed explicitly (`--allow-host`, the MagicDNS name), lowercase, with the port
    /// they are opened on: the listen port unless given (a port forward or a proxy).
    extra: Vec<(String, u16)>,
}

impl HostPolicy {
    /// The policy for a server bound to `listen`, accepting `extra` too: host names or IP
    /// addresses, each with the port it is opened on (`name:port`, `[v6]:port`; the listen
    /// port when left out).
    pub fn new(listen: SocketAddr, extra: &[String]) -> Result<Self> {
        let mut allowed = Vec::new();
        for h in extra {
            let h = h.trim();
            // A bare IPv6 address has colons of its own.
            let (name, port) = match h.parse::<std::net::IpAddr>() {
                Ok(_) => (h, None),
                Err(_) => split_host(h)
                    .with_context(|| format!("--allow-host {h:?}: a host and an optional port"))?,
            };
            let name = canonical(name);
            anyhow::ensure!(
                name.parse::<std::net::IpAddr>().is_ok()
                    || (!name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-.".contains(c))),
                "--allow-host {h:?}: not a host name or IP address"
            );
            allowed.push((name, port.unwrap_or(listen.port())));
        }
        Ok(Self {
            port: listen.port(),
            listen_ip: listen.ip(),
            extra: allowed,
        })
    }

    /// Whether a request whose `Host` header is `host` is answered.
    pub fn allows(&self, host: &str) -> bool {
        let Some((name, port)) = split_host(host) else {
            return false;
        };
        let name = canonical(name);
        // A missing port is the scheme's default: 80, or 443 behind a TLS proxy.
        let on = |p: u16| port.map_or(p == 80 || p == 443, |q| q == p);
        // Explicit entries, on their own port.
        if self.extra.iter().any(|(n, p)| *n == name && on(*p)) {
            return true;
        }
        // Otherwise only on the listen port: the listen address, loopback, any IP literal
        // when listening on every address, and `localhost`.
        if !on(self.port) {
            return false;
        }
        match name.parse::<std::net::IpAddr>() {
            Ok(ip) => ip.is_loopback() || ip == self.listen_ip || self.listen_ip.is_unspecified(),
            Err(_) => name == "localhost",
        }
    }
}

/// A host as compared: lowercase, without a trailing dot, an IP address in its standard form.
fn canonical(name: &str) -> String {
    match name.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => name.trim_end_matches('.').to_ascii_lowercase(),
    }
}

/// `name:port`, `[v6]:port`, `name` or `[v6]` into the name (an IPv6 literal without brackets)
/// and the port; `None` for anything else (a malformed port, a stray bracket).
fn split_host(host: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = host.strip_prefix('[') {
        let (ip, tail) = rest.split_once(']')?;
        if tail.is_empty() {
            return Some((ip, None));
        }
        return Some((ip, Some(tail.strip_prefix(':')?.parse().ok()?)));
    }
    match host.rsplit_once(':') {
        Some((name, port)) => Some((name, Some(port.parse().ok()?))),
        None => Some((host, None)),
    }
}

async fn check_host(
    State(policy): State<Arc<HostPolicy>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    if policy.allows(host) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, format!("host {host:?} not allowed")).into_response()
    }
}

/// This machine's Tailscale IPv4 address and MagicDNS name, from the `tailscale` CLI.
pub fn tailscale_identity() -> Result<(std::net::IpAddr, Option<String>)> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .context("running `tailscale ip -4` (is Tailscale installed and up?)")?;
    anyhow::ensure!(
        out.status.success(),
        "`tailscale ip -4` failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let ip: std::net::IpAddr = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .parse()
        .context("`tailscale ip -4` printed no IPv4 address")?;
    let name = std::process::Command::new("tailscale")
        .args(["status", "--self", "--json"])
        .output()
        .context("running `tailscale status --self --json`")
        .and_then(|o| {
            anyhow::ensure!(
                o.status.success(),
                "`tailscale status --self --json` failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            serde_json::from_slice::<Value>(&o.stdout)
                .context("`tailscale status --self --json` printed no JSON")
        })
        .and_then(|v| {
            v["Self"]["DNSName"]
                .as_str()
                .map(|s| s.trim_end_matches('.').to_string())
                .filter(|s| !s.is_empty())
                .context("`tailscale status` names no MagicDNS name (is MagicDNS on?)")
        });
    let name = match name {
        Ok(n) => Some(n),
        Err(e) => {
            eprintln!(
                "WARNING: no MagicDNS name for this machine ({e:#}): only {ip} is accepted as \
                 Host; add a name with --allow-host"
            );
            None
        }
    };
    Ok((ip, name))
}

/// Every handler answers 200 with a JSON document; failures carry `"error"` so the page
/// can still show whatever was decoded before the failure.
struct ApiResult(Result<Value>);

impl IntoResponse for ApiResult {
    fn into_response(self) -> Response {
        let body = match self.0 {
            Ok(v) => v,
            Err(e) => json!({ "error": format!("{e:#}") }),
        };
        (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(body),
        )
            .into_response()
    }
}

async fn config(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "nodes": st.nodes.iter().map(|n| json!({"name": n.name, "url": n.url.as_str()})).collect::<Vec<_>>(),
        "owner": st.defaults.owner,
        "chainId": st.defaults.chain_id,
        "anchor": st.defaults.anchor,
    }))
}

fn hex0x(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

fn parse_b256(s: &str, what: &str) -> Result<B256> {
    s.trim()
        .parse()
        .with_context(|| format!("{what} must be 32 bytes of hex"))
}

/// Parse a non-negative integer query parameter (taken as a string so a bad value becomes a
/// JSON error instead of axum's plain-text 400).
fn parse_u64(s: Option<&str>, what: &str) -> Result<u64> {
    let s = s
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{what} is required"))?;
    s.parse()
        .with_context(|| format!("{what} must be a non-negative integer, got {s:?}"))
}

fn parse_chain(s: Option<&str>, default: u64) -> Result<u64> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => parse_u64(Some(c), "chain ID"),
        None => Ok(default),
    }
}

fn parse_owner(s: Option<&str>) -> Result<Address> {
    s.filter(|s| !s.trim().is_empty())
        .context("owner address is required")?
        .trim()
        .parse()
        .context("owner must be a 20-byte hex address")
}

// ---------------------------------------------------------------------------------------
// SOC

#[derive(Deserialize)]
struct SocQuery {
    node: Option<String>,
    /// SOC address directly.
    addr: Option<String>,
    /// Or: owner + identifier (hex) …
    owner: Option<String>,
    id: Option<String>,
    /// … or owner + identifier string (hashed with keccak256).
    idstr: Option<String>,
}

async fn soc_handler(State(st): State<Arc<AppState>>, Query(q): Query<SocQuery>) -> ApiResult {
    ApiResult(soc_inspect(&st, q).await)
}

async fn soc_inspect(st: &AppState, q: SocQuery) -> Result<Value> {
    let mut derivation = Value::Null;
    let (address, owner) = if let Some(a) = q.addr.as_deref().filter(|s| !s.trim().is_empty()) {
        // The owner is optional here; an unparsable one is reported, not fatal.
        let owner = match q.owner.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(o) => match parse_owner(Some(o)) {
                Ok(o) => Some(o),
                Err(e) => {
                    derivation = json!({ "ownerError": format!("{e:#} (owner check skipped)") });
                    None
                }
            },
            None => None,
        };
        (parse_b256(a, "SOC address")?, owner)
    } else {
        let owner = parse_owner(q.owner.as_deref())?;
        let id = if let Some(s) = q.idstr.as_deref() {
            if s.is_empty() {
                bail!("identifier string is required");
            }
            let id = keccak256(s.as_bytes());
            derivation = json!({ "identifierString": s, "identifier": id, "owner": owner });
            id
        } else {
            let id = parse_b256(q.id.as_deref().unwrap_or_default(), "identifier")?;
            derivation = json!({ "identifier": id, "owner": owner });
            id
        };
        let addr = soc::soc_address(&id, &owner);
        derivation["address"] = json!(addr);
        (addr, Some(owner))
    };

    let bee = st.bee(q.node.as_deref())?;
    let raw = bee
        .get_chunk(&address)
        .await
        .with_context(|| format!("fetching chunk {address}"))?;
    let mut out = json!({ "address": address, "derivation": derivation, "rawLength": raw.len() });

    let parsed = match soc::parse(&raw) {
        Ok(p) => p,
        Err(e) => {
            out["error"] = json!(format!("not a SOC: {e:#}"));
            out["rawHex"] = json!(hex0x(&raw));
            return Ok(out);
        }
    };
    let address_ok = parsed
        .signer
        .map(|s| soc::soc_address(&parsed.identifier, &s) == address);
    out["soc"] = json!({
        "identifier": parsed.identifier,
        "signature": hex0x(&parsed.signature),
        "span": parsed.span,
        "cacAddress": parsed.cac_address,
        "signer": parsed.signer,
        "signerMatchesAddress": address_ok,
        "signerMatchesOwner": owner.zip(parsed.signer).map(|(o, s)| o == s),
        "payloadLength": parsed.payload.len(),
        "payloadHex": hex0x(&parsed.payload),
    });
    out["record"] = decode_record(&parsed.payload);
    // Verified = signature recovers to an owner whose SOC address is the one fetched, and
    // that owner is the configured one. Without an owner only the first half is checked.
    out["recordVerified"] =
        json!(address_ok == Some(true) && owner.is_some_and(|o| Some(o) == parsed.signer));
    Ok(out)
}

/// Decode an eth2swarm record payload into JSON (best effort).
fn decode_record(payload: &[u8]) -> Value {
    if let Ok(r) = HeaderRecord::decode(payload) {
        let hash = keccak256(&r.header_rlp);
        let header = Header::decode(&mut r.header_rlp.as_slice())
            .map(|h| header_json(&h))
            .unwrap_or_else(|e| json!({ "error": e.to_string() }));
        return json!({
            "kind": "header",
            "geometry": geometry_json(r.geometry),
            "blockHash": hash,
            "header": header,
            "headerRlpLength": r.header_rlp.len(),
            "headerRlpHex": hex0x(&r.header_rlp),
            "bodyRef": r.body_ref,
            "epochProof": r.epoch_proof,
            "masterProof": r.master_proof,
        });
    }
    if let Ok(r) = EpochRecord::decode(payload) {
        return json!({
            "kind": "epoch",
            "geometry": geometry_json(r.geometry),
            "epochRoot": r.epoch_root,
            "headersRef": r.headers_ref,
            "masterProof": r.master_proof,
        });
    }
    if let Ok(r) = AnchorRecord::decode(payload) {
        return json!({
            "kind": "anchor",
            "geometry": geometry_json(r.geometry),
            "anchor": r.anchor,
        });
    }
    if let Ok(h) = HeadRecord::decode(payload) {
        return json!({ "kind": "addrHead", "head": head_json(&h) });
    }
    if let Ok(r) = SliceRecord::decode(payload) {
        return json!({ "kind": "addrSlice", "slice": slice_json(&r) });
    }
    // A bucket's entries are relative to its slice's first block, which only its descriptor
    // states; decoded here from the slice's grid start (the Address tab decodes it properly).
    if let Ok(b) = BucketRecord::decode_at_grid_start(payload) {
        return json!({
            "kind": "addrBucket",
            "bucket": bucket_json(&b, None),
            "note": "entries decoded from the slice's grid start; in an index from a later \
                     block the slice the start falls into starts later (use the Address tab)",
        });
    }
    json!({ "kind": "unknown" })
}

// ---------------------------------------------------------------------------------------
// Address index (addr1, docs/address-index.md)

#[derive(Deserialize)]
struct AddrQuery {
    node: Option<String>,
    owner: Option<String>,
    chain: Option<String>,
    /// The head announcing this many units (blank: the newest found by probing).
    units: Option<String>,
    address: Option<String>,
    since: Option<String>,
    level: Option<String>,
    index: Option<String>,
    k: Option<String>,
    r#ref: Option<String>,
    count: Option<String>,
    /// Entries returned at most (the count is always the full one).
    limit: Option<String>,
}

/// Entries a view returns by default: a busy contract has millions.
const ENTRY_LIMIT: usize = 500;

fn opt_u64(s: Option<&str>, what: &str) -> Result<Option<u64>> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => parse_u64(Some(v), what).map(Some),
        None => Ok(None),
    }
}

/// The query's parameters, parsed before anything is read, so a typo is reported at once.
struct AddrParams {
    owner: Address,
    chain_id: u64,
    units: Option<u64>,
    address: Option<Address>,
    since: Option<u64>,
    limit: usize,
}

impl AddrParams {
    fn parse(st: &AppState, q: &AddrQuery) -> Result<Self> {
        Ok(Self {
            owner: parse_owner(q.owner.as_deref())?,
            chain_id: parse_chain(q.chain.as_deref(), st.defaults.chain_id)?,
            units: opt_u64(q.units.as_deref(), "units")?,
            address: q
                .address
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|a| a.parse().context("address must be 20 bytes of hex"))
                .transpose()?,
            since: opt_u64(q.since.as_deref(), "since")?,
            limit: opt_u64(q.limit.as_deref(), "limit")?.map_or(ENTRY_LIMIT, |l| l as usize),
        })
    }
}

fn head_json(h: &HeadRecord) -> Value {
    json!({
        "firstBlock": h.first_block(),
        "units": h.units,
        "lastBlock": h.last_block(),
        "lastBlockHash": h.last_block_hash,
        "lastBlockTimestamp": h.last_block_timestamp,
        "pending": h.pending.iter().map(|(l, i)| json!([l, i])).collect::<Vec<_>>(),
    })
}

fn slice_json(r: &SliceRecord) -> Value {
    json!({
        "level": r.level,
        "index": r.index,
        "firstBlock": r.first_block,
        "lastBlock": r.last_block(),
        "lastBlockHash": r.last_block_hash,
        "bucketCount": r.bucket_count,
        "addresses": r.addresses,
        "appearances": r.appearances,
        "bucketsRoot": r.buckets_root,
        "roles": role::names(r.roles),
    })
}

fn entry_json(e: &Entry) -> Value {
    json!({
        "block": e.block,
        "slot": e.slot,
        // Slot 0 is the block itself (beneficiary, withdrawals, uncles); slot i the
        // transaction with index i − 1.
        "tx": e.slot.checked_sub(1),
        "roles": role::names(e.roles),
    })
}

fn records_json(recs: &[AddressRecord], highlight: Option<[u8; 8]>) -> Vec<Value> {
    recs.iter()
        .map(|r| {
            let (n, spill, inline) = match &r.entries {
                Entries::Inline(e) => (
                    e.len() as u64,
                    Value::Null,
                    e.iter().map(entry_json).collect::<Vec<_>>(),
                ),
                Entries::Spilled { count, spill_ref } => (*count, json!(spill_ref), Vec::new()),
            };
            json!({
                "fp": hex0x(&r.fp),
                "entries": n,
                "spillRef": spill,
                "inline": inline,
                "match": highlight == Some(r.fp),
            })
        })
        .collect()
}

fn bucket_json(b: &BucketRecord, highlight: Option<[u8; 8]>) -> Value {
    match &b.body {
        BucketBody::Inline(recs) => json!({
            "level": b.level, "index": b.index, "bucket": b.bucket,
            "paged": false, "records": records_json(recs, highlight),
        }),
        BucketBody::Paged {
            count,
            pages,
            pages_ref,
        } => json!({
            "level": b.level, "index": b.index, "bucket": b.bucket,
            "paged": true, "count": count, "pages": pages, "pagesRef": pages_ref,
        }),
    }
}

/// The reader for the parameters' owner and chain on the query's node, and the origin. A
/// missing origin is the usual sign of a wrong owner, chain or node, which the error says.
async fn index_origin(
    st: &AppState,
    q: &AddrQuery,
    p: &AddrParams,
) -> Result<(IndexReader, HeadRecord)> {
    let r = IndexReader {
        bee: st.bee(q.node.as_deref())?,
        owner: p.owner,
        chain_id: p.chain_id,
    };
    let origin = r.origin().await.with_context(|| {
        format!(
            "no address index of owner {} on chain {} could be read from this node (wrong \
             owner, chain ID or node, or not retrievable)",
            p.owner, p.chain_id
        )
    })?;
    Ok((r, origin))
}

/// The head to read: the one with the parameters' units, or the newest found by probing head
/// updates (the inspector has no RPC), starting from the newest this inspector found before
/// for the same node, owner and chain; and a description of it, with the slices a lookup since
/// unit `since` reads.
async fn index_head(
    st: &AppState,
    q: &AddrQuery,
    r: &IndexReader,
    origin: &HeadRecord,
    p: &AddrParams,
) -> Result<(HeadRecord, Value)> {
    let key = (q.node.clone().unwrap_or_default(), r.owner, r.chain_id);
    let (head, how, miss) = match p.units {
        Some(u) => (
            r.head_at(origin, u).await?,
            "as requested".to_string(),
            None,
        ),
        None => {
            let mut known = st.heads.lock().ok().and_then(|m| m.get(&key).copied());
            let found = match newest_head_by_probing(r, origin, known.unwrap_or(0)).await {
                // The update found before cannot be read (the node lost it, or the index was
                // published again under this owner): forget it and search from update 0.
                Err(_) if known.is_some() => {
                    known = None;
                    newest_head_by_probing(r, origin, 0).await
                }
                found => found,
            };
            let (head, probes, miss) = found?;
            let n = head.units - origin.units;
            if let Ok(mut m) = st.heads.lock() {
                m.insert(key, n);
            }
            let from = match known {
                Some(k) => format!("update {k}, the newest found before"),
                None => "update 0".to_string(),
            };
            (
                head,
                format!("newest found: update {n}, after {probes} probes from {from}"),
                miss,
            )
        }
    };
    let (slices, _) = read_plan(&head, p.since)?;
    let plan: Vec<Value> = slices
        .into_iter()
        .rev()
        .map(|(l, i)| json!([l, i]))
        .collect();
    let index = json!({
        "owner": r.owner,
        "chainId": r.chain_id,
        "origin": head_json(origin),
        "head": head_json(&head),
        "headUpdate": head.units - origin.units,
        "headFound": how,
        // Why the probe stopped where it did: usually the next update is not published yet,
        // but Bee answers a missing and an unretrievable chunk alike.
        "lastMiss": miss,
        "slices": plan,
    });
    Ok((head, index))
}

/// What the slice, bucket and spill views show of the index: no head is probed, only the
/// origin is read (its start fixes where each slice starts).
fn origin_json(r: &IndexReader, origin: &HeadRecord) -> Value {
    json!({ "owner": r.owner, "chainId": r.chain_id, "origin": head_json(origin) })
}

/// Runs `steps`, which fill `out` as they go: an error is added to what they had filled
/// (the page shows both), as the block view does.
async fn partial<F>(mut out: Value, steps: F) -> Result<Value>
where
    F: AsyncFnOnce(&mut Value) -> Result<()>,
{
    if let Err(e) = steps(&mut out).await {
        out["error"] = json!(format!("{e:#}"));
    }
    Ok(out)
}

async fn addr_lookup_handler(
    State(st): State<Arc<AppState>>,
    Query(q): Query<AddrQuery>,
) -> ApiResult {
    ApiResult(addr_lookup_inspect(&st, q).await)
}

async fn addr_lookup_inspect(st: &AppState, q: AddrQuery) -> Result<Value> {
    let p = AddrParams::parse(st, &q)?;
    partial(json!({}), async |out| {
        let (r, origin) = index_origin(st, &q, &p).await?;
        let (head, index) = index_head(st, &q, &r, &origin, &p).await?;
        out["index"] = index;
        let Some(address) = p.address else {
            return Ok(());
        };
        out["address"] = json!(address);
        let found = addr_lookup(&r, address, &head, p.since).await?;
        out["lookup"] = json!({
            "fromBlock": found.first_block,
            "toBlock": head.last_block(),
            "slices": found.slices,
            "files": found.files,
            "roles": role::names(found.roles),
            "count": found.entries.len(),
            "shown": found.entries.len().min(p.limit),
            "entries": found.entries.iter().take(p.limit).map(entry_json).collect::<Vec<_>>(),
        });
        Ok(())
    })
    .await
}

fn slice_param(q: &AddrQuery) -> Result<SliceId> {
    let level = parse_u64(q.level.as_deref(), "level")?;
    let index = parse_u64(q.index.as_deref(), "index")?;
    anyhow::ensure!(
        level <= u64::from(crate::addr::layout::L_MAX),
        "level {level} above {}",
        crate::addr::layout::L_MAX
    );
    Ok((
        level as u8,
        u32::try_from(index).context("index out of range")?,
    ))
}

/// A slice id from the query must exist in the index: end after the origin's start.
fn check_exists(s: SliceId, origin: &HeadRecord) -> Result<()> {
    anyhow::ensure!(
        !held(s, origin.first_unit).is_empty(),
        "slice ({}, {}) ends before the index starts at block {}",
        s.0,
        s.1,
        origin.first_block()
    );
    Ok(())
}

async fn addr_slice_handler(
    State(st): State<Arc<AppState>>,
    Query(q): Query<AddrQuery>,
) -> ApiResult {
    ApiResult(addr_slice_inspect(&st, q).await)
}

async fn addr_slice_inspect(st: &AppState, q: AddrQuery) -> Result<Value> {
    let p = AddrParams::parse(st, &q)?;
    let s = slice_param(&q)?;
    partial(json!({}), async |out| {
        let (r, origin) = index_origin(st, &q, &p).await?;
        out["index"] = origin_json(&r, &origin);
        check_exists(s, &origin)?;
        let rec = r.slice(s, origin.first_unit).await?;
        out["slice"] = slice_json(&rec);
        if let Some(address) = p.address {
            let h = keccak256(address.as_slice());
            out["addressBucket"] = json!(bucket_of(&h, rec.bucket_count));
            out["address"] = json!(address);
        }
        Ok(())
    })
    .await
}

async fn addr_bucket_handler(
    State(st): State<Arc<AppState>>,
    Query(q): Query<AddrQuery>,
) -> ApiResult {
    ApiResult(addr_bucket_inspect(&st, q).await)
}

async fn addr_bucket_inspect(st: &AppState, q: AddrQuery) -> Result<Value> {
    let p = AddrParams::parse(st, &q)?;
    let s = slice_param(&q)?;
    let k = u32::try_from(parse_u64(q.k.as_deref(), "bucket")?).context("bucket out of range")?;
    let highlight = p.address.map(|a| fingerprint(&keccak256(a.as_slice())));
    partial(json!({}), async |out| {
        let (r, origin) = index_origin(st, &q, &p).await?;
        out["index"] = origin_json(&r, &origin);
        check_exists(s, &origin)?;
        let rec = r.slice(s, origin.first_unit).await?;
        out["slice"] = slice_json(&rec);
        anyhow::ensure!(
            k < rec.bucket_count,
            "slice ({}, {}) has {} buckets",
            s.0,
            s.1,
            rec.bucket_count
        );
        // One read of the bucket, and of a paged bucket's pages file, all checked.
        let (payload, records, _) = r.bucket_records(&rec, k).await?;
        let bucket = BucketRecord::decode(&payload, rec.first_block)?;
        out["bucket"] = bucket_json(&bucket, highlight);
        if matches!(bucket.body, BucketBody::Paged { .. }) {
            out["pagedRecords"] = json!(records_json(&records, highlight));
        }
        Ok(())
    })
    .await
}

/// Well-known addresses to try per chain. The index holds fingerprints, not addresses, so it
/// cannot list its own; these are checked against it instead ([`addr_examples_inspect`]).
fn example_addresses(chain_id: u64) -> &'static [(&'static str, &'static str)] {
    match chain_id {
        1 => &[
            ("0xdAC17F958D2ee523a2206206994597C13D831ec7", "USDT"),
            ("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", "USDC"),
            ("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", "WETH"),
            ("0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84", "Lido stETH"),
            (
                "0x00000000219ab540356cBB839Cbe05303d7705Fa",
                "beacon deposit contract",
            ),
            (
                "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D",
                "Uniswap V2 router",
            ),
            ("0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045", "vitalik.eth"),
            (
                "0x95222290DD7278Aa3Ddd389Cc1E1d165CC4BAfe5",
                "beaverbuild (block builder)",
            ),
            (
                "0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97",
                "Titan (block builder)",
            ),
        ],
        // Anvil / Hardhat dev accounts #0–#2 (bee-factory's chain is 1337).
        1337 | 31337 => &[
            (
                "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
                "dev account #0",
            ),
            (
                "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
                "dev account #1",
            ),
            (
                "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
                "dev account #2",
            ),
        ],
        _ => &[],
    }
}

async fn addr_examples_handler(
    State(st): State<Arc<AppState>>,
    Query(q): Query<AddrQuery>,
) -> ApiResult {
    ApiResult(addr_examples_inspect(&st, q).await)
}

/// How many entries each example address has in the index as of the head, from the address
/// records alone: a descriptor and one bucket per slice (a paged bucket with its pages file),
/// never a spill file, so even a token contract with millions of entries is counted quickly.
/// A count can include another address with the same 64-bit fingerprint (rare); a lookup
/// shows the entries.
async fn addr_examples_inspect(st: &AppState, q: AddrQuery) -> Result<Value> {
    // The whole index: no `since`, and no address of the query's.
    let p = AddrParams {
        since: None,
        address: None,
        ..AddrParams::parse(st, &q)?
    };
    partial(json!({}), async |out| {
        let (r, origin) = index_origin(st, &q, &p).await?;
        let (head, index) = index_head(st, &q, &r, &origin, &p).await?;
        out["index"] = index;
        let examples: Vec<(Address, &str)> = example_addresses(r.chain_id)
            .iter()
            .map(|(a, label)| Ok((a.parse()?, *label)))
            .collect::<Result<_>>()?;
        out["examples"] = json!([]);
        if examples.is_empty() {
            return Ok(());
        }
        let (slices, _) = read_plan(&head, None)?;
        let first_unit = head.first_unit;
        let reads: Vec<_> = slices
            .iter()
            .map(|s| {
                let (r, s) = (r.clone(), *s);
                async move { r.slice(s, first_unit).await }
            })
            .collect();
        let descriptors = bounded(reads)
            .await?
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        // Each distinct (slice, bucket) the addresses fall into is read once; an address whose
        // bucket cannot be read gets the error, the others are still counted.
        let hashes: Vec<B256> = examples
            .iter()
            .map(|(a, _)| keccak256(a.as_slice()))
            .collect();
        let mut wanted: Vec<(usize, u32)> = descriptors
            .iter()
            .enumerate()
            .flat_map(|(i, rec)| {
                hashes
                    .iter()
                    .map(move |h| (i, bucket_of(h, rec.bucket_count)))
            })
            .collect();
        wanted.sort();
        wanted.dedup();
        let descriptors = Arc::new(descriptors);
        let reads: Vec<_> = wanted
            .iter()
            .map(|&(i, k)| {
                let (r, descriptors) = (r.clone(), descriptors.clone());
                async move {
                    let (_, records, _) = r.bucket_records(&descriptors[i], k).await?;
                    Ok::<_, anyhow::Error>(records)
                }
            })
            .collect();
        let buckets: std::collections::HashMap<(usize, u32), Result<Vec<AddressRecord>>> =
            wanted.into_iter().zip(bounded(reads).await?).collect();
        let rows: Vec<Value> = examples
            .iter()
            .zip(&hashes)
            .map(|((a, label), h)| {
                let fp = fingerprint(h);
                let mut count = 0u64;
                for (i, rec) in descriptors.iter().enumerate() {
                    match &buckets[&(i, bucket_of(h, rec.bucket_count))] {
                        Ok(records) => {
                            for x in records.iter().filter(|x| x.fp == fp) {
                                count += match &x.entries {
                                    Entries::Inline(e) => e.len() as u64,
                                    Entries::Spilled { count, .. } => *count,
                                };
                            }
                        }
                        Err(e) => {
                            return json!({ "address": a, "label": label, "entries": null,
                                           "error": format!("{e:#}") });
                        }
                    }
                }
                json!({ "address": a, "label": label, "entries": count })
            })
            .collect();
        out["examples"] = json!(rows);
        Ok(())
    })
    .await
}

/// Runs `jobs`, at most [`CONCURRENCY`] at a time, and returns their results in order; a job
/// that panicked is an error (the request still answers with JSON).
async fn bounded<T, F>(jobs: impl IntoIterator<Item = F>) -> Result<Vec<T>>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut jobs = jobs.into_iter().enumerate();
    let mut out: Vec<Option<T>> = Vec::new();
    let mut set = tokio::task::JoinSet::new();
    loop {
        while set.len() < CONCURRENCY {
            let Some((i, job)) = jobs.next() else { break };
            out.push(None);
            set.spawn(async move { (i, job.await) });
        }
        let Some(done) = set.join_next().await else {
            break;
        };
        let (i, v) = done.context("a read panicked")?;
        out[i] = Some(v);
    }
    Ok(out.into_iter().map(|v| v.expect("every job ran")).collect())
}

async fn addr_spill_handler(
    State(st): State<Arc<AppState>>,
    Query(q): Query<AddrQuery>,
) -> ApiResult {
    ApiResult(addr_spill_inspect(&st, q).await)
}

async fn addr_spill_inspect(st: &AppState, q: AddrQuery) -> Result<Value> {
    let p = AddrParams::parse(st, &q)?;
    let s = slice_param(&q)?;
    let spill_ref = parse_b256(q.r#ref.as_deref().unwrap_or_default(), "spill reference")?;
    let count = parse_u64(q.count.as_deref(), "entry count")?;
    partial(json!({}), async |out| {
        let (r, origin) = index_origin(st, &q, &p).await?;
        out["index"] = origin_json(&r, &origin);
        check_exists(s, &origin)?;
        let rec = r.slice(s, origin.first_unit).await?;
        out["slice"] = slice_json(&rec);
        let entries = r.spill_entries(&rec, &spill_ref, count).await?;
        // Newest first, as the lookup lists them: the file holds them oldest first.
        out["spillRef"] = json!(spill_ref);
        out["count"] = json!(entries.len());
        out["shown"] = json!(entries.len().min(p.limit));
        out["entries"] = json!(
            entries
                .iter()
                .rev()
                .take(p.limit)
                .map(entry_json)
                .collect::<Vec<_>>()
        );
        Ok(())
    })
    .await
}

fn geometry_json(g: Geometry) -> Value {
    json!({ "epochSize": g.epoch_size, "numEpochs": g.num_epochs,
            "blocks": u64::from(g.epoch_size) * u64::from(g.num_epochs) })
}

fn header_json(h: &Header) -> Value {
    json!({
        "number": h.number,
        "parentHash": h.parent_hash,
        "timestamp": h.timestamp,
        "beneficiary": h.beneficiary,
        "stateRoot": h.state_root,
        "transactionsRoot": h.transactions_root,
        "receiptsRoot": h.receipts_root,
        "ommersHash": h.ommers_hash,
        "logsBloomNonZero": h.logs_bloom.0.iter().any(|b| *b != 0),
        "difficulty": h.difficulty.to_string(),
        "gasLimit": h.gas_limit,
        "gasUsed": h.gas_used,
        "extraData": hex0x(&h.extra_data),
        "extraDataText": String::from_utf8(h.extra_data.to_vec()).ok().filter(|s| s.chars().all(|c| !c.is_control())),
        "mixHash": h.mix_hash,
        "nonce": h.nonce,
        "baseFeePerGas": h.base_fee_per_gas,
        "withdrawalsRoot": h.withdrawals_root,
        "blobGasUsed": h.blob_gas_used,
        "excessBlobGas": h.excess_blob_gas,
        "parentBeaconBlockRoot": h.parent_beacon_block_root,
        "requestsHash": h.requests_hash,
        "fieldCount": header_field_count(h),
    })
}

fn tx_json(tx: &TxEnvelope) -> Value {
    json!({
        "hash": tx.tx_hash(),
        "type": tx.ty(),
        "from": tx.recover_signer().ok(),
        "to": tx.to(),
        "nonce": tx.nonce(),
        "value": tx.value().to_string(),
        "gasLimit": tx.gas_limit(),
        "inputLength": tx.input().len(),
    })
}

fn body_json(body: &Body) -> Value {
    json!({
        "transactions": body.transactions.iter().map(tx_json).collect::<Vec<_>>(),
        "ommers": body.ommers.iter().map(|o| json!({
            "hash": keccak256(alloy::rlp::encode(o)),
            "number": o.number,
            "beneficiary": o.beneficiary,
        })).collect::<Vec<_>>(),
        "withdrawals": body.withdrawals.as_ref().map(|w| w.len()),
    })
}

// ---------------------------------------------------------------------------------------
// Block (full verification, step by step)

#[derive(Deserialize)]
struct BlockQuery {
    node: Option<String>,
    owner: Option<String>,
    chain: Option<String>,
    anchor: Option<String>,
    n: Option<String>,
}

async fn block_handler(State(st): State<Arc<AppState>>, Query(q): Query<BlockQuery>) -> ApiResult {
    ApiResult(block_inspect(&st, q).await)
}

/// Records each verification step; the first failure stops the run but keeps the report.
struct Steps(Vec<Value>);

impl Steps {
    fn check(&mut self, name: &str, ok: bool, detail: impl Into<String>) -> Result<()> {
        let detail = detail.into();
        self.0
            .push(json!({ "step": name, "ok": ok, "detail": detail }));
        if ok {
            Ok(())
        } else {
            bail!("{name}: {detail}")
        }
    }
    fn info(&mut self, name: &str, detail: impl Into<String>) {
        self.0
            .push(json!({ "step": name, "ok": Value::Null, "detail": detail.into() }));
    }
}

async fn resolve_anchor(
    bee: &Bee,
    owner: &Address,
    chain: u64,
    anchor: Option<&str>,
    steps: &mut Steps,
) -> Result<(B256, bool)> {
    if let Some(a) = anchor.filter(|s| !s.trim().is_empty()) {
        let a = parse_b256(a, "anchor")?;
        steps.info("trust anchor", format!("pinned {a}"));
        return Ok((a, true));
    }
    let id = layout::anchor_id(chain);
    let addr = soc::soc_address(&id, owner);
    let raw = bee
        .get_chunk(&addr)
        .await
        .with_context(|| format!("fetching published anchor SOC {addr}"))?;
    let payload = soc::verify(&raw, &addr, owner)?;
    let rec = AnchorRecord::decode(&payload)?;
    steps.info(
        "trust anchor",
        format!(
            "NOT pinned — using the owner's published anchor {} (trusts the publisher)",
            rec.anchor
        ),
    );
    Ok((rec.anchor, false))
}

async fn block_inspect(st: &AppState, q: BlockQuery) -> Result<Value> {
    let bee = st.bee(q.node.as_deref())?;
    let owner = parse_owner(q.owner.as_deref())?;
    let chain = parse_chain(q.chain.as_deref(), st.defaults.chain_id)?;
    let n = parse_u64(q.n.as_deref(), "block number")?;
    let mut steps = Steps(Vec::new());
    let mut out = json!({ "number": n, "chainId": chain, "owner": owner });

    let result: Result<()> = async {
        let (anchor, pinned) =
            resolve_anchor(&bee, &owner, chain, q.anchor.as_deref(), &mut steps).await?;
        out["anchor"] = json!(anchor);
        out["anchorPinned"] = json!(pinned);

        let idstr = format!("eth:{chain}:{anchor}:hdr:{}", n);
        let id = layout::header_id(chain, &anchor, n);
        let addr = soc::soc_address(&id, &owner);
        out["soc"] = json!({ "identifierString": idstr, "identifier": id, "address": addr });
        let raw = match bee.get_chunk(&addr).await {
            Ok(r) => r,
            Err(e) => {
                let mut msg = format!("{e:#}");
                if msg.contains("404") {
                    msg.push_str(&format!(
                        " — no SOC at {addr}: wrong owner, chain ID or anchor, a block \
                         outside the imported range, or not (yet) available on this node"
                    ));
                }
                steps.check("fetch header SOC", false, msg)?;
                unreachable!()
            }
        };
        steps.check(
            "fetch header SOC",
            true,
            format!("{addr} ({} bytes)", raw.len()),
        )?;

        let parsed = soc::parse(&raw)?;
        steps.check(
            "SOC signature",
            parsed.signer == Some(owner) && soc::soc_address(&parsed.identifier, &owner) == addr,
            format!(
                "signed by {}",
                parsed
                    .signer
                    .map_or("<invalid signature>".to_string(), |s| s.to_string())
            ),
        )?;

        let rec = HeaderRecord::decode(&parsed.payload)?;
        out["record"] = decode_record(&parsed.payload);
        let hash = keccak256(&rec.header_rlp);
        let header = Header::decode(&mut rec.header_rlp.as_slice()).context("decoding header")?;
        steps.check(
            "header number",
            header.number == n,
            format!("header #{} (hash {hash})", header.number),
        )?;

        let g = rec.geometry;
        let size = u64::from(g.epoch_size);
        steps.check(
            "geometry",
            g.epoch_size.is_power_of_two() && n / size < u64::from(g.num_epochs),
            format!(
                "{} epochs × {} blocks; block is in epoch {}",
                g.num_epochs,
                g.epoch_size,
                n / size.max(1)
            ),
        )?;
        let (epoch, index) = ((n / size) as usize, (n % size) as usize);
        let epoch_depth = accumulator::depth_for(g.epoch_size as usize) as usize;
        let master_depth = accumulator::depth_for(g.num_epochs as usize) as usize;
        steps.check(
            "proof lengths",
            rec.epoch_proof.len() == epoch_depth && rec.master_proof.len() == master_depth,
            format!(
                "epoch proof {} (want {epoch_depth}), master proof {} (want {master_depth})",
                rec.epoch_proof.len(),
                rec.master_proof.len()
            ),
        )?;
        let epoch_root = root_from_proof(hash, index, &rec.epoch_proof);
        let master_root = root_from_proof(epoch_root, epoch, &rec.master_proof);
        let derived = accumulator::anchor(master_root, g.num_epochs.into(), g.epoch_size.into());
        steps.check(
            "accumulator proof → anchor",
            derived == anchor,
            format!("epoch root {epoch_root}, proof leads to anchor {derived}"),
        )?;

        let body_rlp = match bee.get_bytes(&rec.body_ref, 16 << 20).await {
            Ok(b) => b,
            Err(e) => {
                steps.check("fetch body", false, format!("{e:#}"))?;
                unreachable!()
            }
        };
        steps.check(
            "fetch body",
            true,
            format!("{} ({} bytes)", rec.body_ref, body_rlp.len()),
        )?;
        let mut buf = body_rlp.as_slice();
        let body = Body::decode(&mut buf).context("decoding body")?;
        let (ok, detail) = match check_body(&header, &body) {
            Err(e) => (false, format!("{e:#}")),
            Ok(()) if !buf.is_empty() => (
                false,
                format!("{} trailing bytes after the body", buf.len()),
            ),
            Ok(()) => (
                true,
                "transactionsRoot, ommersHash and withdrawalsRoot match".to_string(),
            ),
        };
        steps.check("body matches header roots", ok, detail)?;
        out["body"] = body_json(&body);
        Ok(())
    }
    .await;

    out["verified"] = json!(result.is_ok());
    if let Err(e) = result {
        out["error"] = json!(format!("{e:#}"));
    }
    out["steps"] = json!(steps.0);
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Epoch

#[derive(Deserialize)]
struct EpochQuery {
    node: Option<String>,
    owner: Option<String>,
    chain: Option<String>,
    anchor: Option<String>,
    k: Option<String>,
}

async fn epoch_handler(State(st): State<Arc<AppState>>, Query(q): Query<EpochQuery>) -> ApiResult {
    ApiResult(epoch_inspect(&st, q).await)
}

async fn epoch_inspect(st: &AppState, q: EpochQuery) -> Result<Value> {
    let bee = st.bee(q.node.as_deref())?;
    let owner = parse_owner(q.owner.as_deref())?;
    let chain = parse_chain(q.chain.as_deref(), st.defaults.chain_id)?;
    let k = parse_u64(q.k.as_deref(), "epoch")?;
    let mut steps = Steps(Vec::new());
    let (anchor, pinned) =
        resolve_anchor(&bee, &owner, chain, q.anchor.as_deref(), &mut steps).await?;
    let client = crate::client::Client {
        bee: &bee,
        owner,
        chain_id: chain,
    };
    let id = layout::epoch_id(chain, &anchor, k);
    let addr = soc::soc_address(&id, &owner);
    let mut out = json!({
        "epoch": k, "anchor": anchor, "anchorPinned": pinned,
        "soc": { "identifierString": format!("eth:{chain}:{anchor}:epoch:{}", k), "identifier": id, "address": addr },
    });
    if let Ok(raw) = bee.get_chunk(&addr).await {
        match soc::verify(&raw, &addr, &owner) {
            Ok(payload) => {
                out["record"] = decode_record(&payload);
                out["recordVerified"] = json!(true);
            }
            Err(_) => {
                if let Ok(p) = soc::parse(&raw) {
                    out["record"] = decode_record(&p.payload);
                    out["recordVerified"] = json!(false);
                }
            }
        }
    }
    match client.verify_epoch(k, anchor).await {
        Ok(headers) => {
            steps.check(
                "epoch file",
                true,
                format!(
                    "{} headers: linkage, numbering and epoch root verified against the anchor",
                    headers.len()
                ),
            )?;
            out["verified"] = json!(true);
            out["headers"] = json!(
                headers
                    .iter()
                    .map(|h| json!({
                        "number": h.number,
                        "hash": keccak256(alloy::rlp::encode(h)),
                        "timestamp": h.timestamp,
                        "gasUsed": h.gas_used,
                        "beneficiary": h.beneficiary,
                        "bodyContents": body_contents(h),
                    }))
                    .collect::<Vec<_>>()
            );
        }
        Err(e) => {
            steps
                .0
                .push(json!({ "step": "epoch file", "ok": false, "detail": format!("{e:#}") }));
            out["verified"] = json!(false);
            out["error"] = json!(format!("{e:#}"));
        }
    }
    out["steps"] = json!(steps.0);
    Ok(out)
}

/// What a header says its body holds, from its roots alone (empty list = empty body).
fn body_contents(h: &Header) -> Vec<&'static str> {
    use alloy::consensus::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH};
    let mut parts = Vec::new();
    if h.transactions_root != EMPTY_ROOT_HASH {
        parts.push("txs");
    }
    if h.ommers_hash != EMPTY_OMMER_ROOT_HASH {
        parts.push("uncles");
    }
    // Post-Shanghai: a block with no transactions can still carry withdrawals.
    if h.withdrawals_root.is_some_and(|r| r != EMPTY_ROOT_HASH) {
        parts.push("withdrawals");
    }
    parts
}

// ---------------------------------------------------------------------------------------
// Raw bytes reference

#[derive(Deserialize)]
struct BytesQuery {
    node: Option<String>,
    r#ref: String,
}

async fn bytes_handler(State(st): State<Arc<AppState>>, Query(q): Query<BytesQuery>) -> ApiResult {
    ApiResult(bytes_inspect(&st, q).await)
}

async fn bytes_inspect(st: &AppState, q: BytesQuery) -> Result<Value> {
    let bee = st.bee(q.node.as_deref())?;
    let reference = parse_b256(&q.r#ref, "reference")?;
    let data = bee.get_bytes(&reference, 64 << 20).await?;
    let mut out = json!({
        "reference": reference,
        "length": data.len(),
        "chunks": data.len().div_ceil(CHUNK_SIZE).max(1),
        "hexPreview": hex0x(&data[..data.len().min(2048)]),
        "truncated": data.len() > 2048,
    });

    // Try: a block body …
    let mut buf = data.as_slice();
    if let Ok(body) = Body::decode(&mut buf)
        && buf.is_empty()
    {
        out["decodedAs"] = json!("block body");
        out["body"] = body_json(&body);
        return Ok(out);
    }
    // … or a sequence of headers (an epoch file).
    let mut buf = data.as_slice();
    let mut headers = Vec::new();
    while !buf.is_empty() {
        let start = buf;
        match Header::decode(&mut buf) {
            Ok(h) => headers.push(json!({
                "number": h.number,
                "hash": keccak256(&start[..start.len() - buf.len()]),
                "timestamp": h.timestamp,
            })),
            Err(_) => {
                headers.clear();
                break;
            }
        }
    }
    if !headers.is_empty() {
        out["decodedAs"] = json!("header sequence");
        out["headers"] = json!(headers);
    } else {
        out["decodedAs"] = json!(Value::Null);
        const TEXT_MAX: usize = 64 * 1024;
        if let Ok(text) = std::str::from_utf8(&data[..data.len().min(TEXT_MAX)]) {
            out["text"] = json!(text);
            out["textTruncated"] = json!(data.len() > TEXT_MAX);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::consensus::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH};

    fn header() -> Header {
        Header {
            transactions_root: EMPTY_ROOT_HASH,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            ..Default::default()
        }
    }

    #[test]
    fn example_addresses_parse() {
        for chain in [1, 1337, 31337] {
            let list = example_addresses(chain);
            assert!(!list.is_empty(), "chain {chain}");
            for (a, label) in list {
                let parsed: Address = a.parse().unwrap_or_else(|e| panic!("{label}: {e}"));
                // Mixed-case addresses carry an EIP-55 checksum: a typo fails it.
                if a[2..].chars().any(|c| c.is_ascii_uppercase()) {
                    assert_eq!(parsed.to_checksum(None), *a, "{label}");
                }
            }
        }
        assert!(example_addresses(5).is_empty());
    }

    #[test]
    fn host_policy() {
        let policy = |listen: &str, extra: &[&str]| {
            let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
            HostPolicy::new(listen.parse().unwrap(), &extra).unwrap()
        };
        let lo = policy("127.0.0.1:8090", &[]);
        assert!(
            lo.allows("127.0.0.1:8090") && lo.allows("localhost:8090") && lo.allows("[::1]:8090")
        );
        assert!(!lo.allows("evil.example:8090") && !lo.allows("127.0.0.1:9999"));
        assert!(
            !lo.allows("100.64.0.1:8090"),
            "another IP when bound to loopback"
        );
        // On a Tailscale address with its MagicDNS name.
        let ts = policy("100.64.0.1:8090", &["nuc.example.ts.net."]);
        assert!(ts.allows("100.64.0.1:8090") && ts.allows("NUC.example.ts.net:8090"));
        assert!(!ts.allows("192.168.1.5:8090") && !ts.allows("rebind.attacker.example:8090"));
        // On every address: any IP literal, still no unknown name.
        let any = policy("0.0.0.0:8090", &[]);
        assert!(any.allows("192.168.1.5:8090") && any.allows("[fe80::1]:8090"));
        assert!(!any.allows("rebind.attacker.example:8090") && !any.allows("192.168.1.5"));
        // Port 80 may be left out.
        let p80 = policy("0.0.0.0:80", &[]);
        assert!(p80.allows("192.168.1.5") && p80.allows("localhost"));
        // Further IP addresses (a port forward), and malformed ports or brackets refused.
        let fwd = policy("127.0.0.1:8090", &["192.168.1.5", "[fe80::2]"]);
        assert!(fwd.allows("192.168.1.5:8090") && fwd.allows("[fe80::2]:8090"));
        assert!(!fwd.allows("192.168.1.6:8090"));
        for bad in [
            "localhost:abc",
            "localhost:",
            "[::1]x",
            "[::1",
            "[::1]:99999",
        ] {
            assert!(!lo.allows(bad), "{bad}");
        }
        // Forwards and proxies on another port: `ssh -L 9000:127.0.0.1:8090`, a router or
        // docker forward, a TLS proxy on 443 passing the Host on (no port in it).
        let fwd = policy(
            "127.0.0.1:8090",
            &[
                "localhost:9000",
                "192.168.1.5:9000",
                "inspector.example:443",
                "fe80::3",
            ],
        );
        assert!(fwd.allows("localhost:9000") && fwd.allows("192.168.1.5:9000"));
        assert!(fwd.allows("inspector.example") && fwd.allows("[fe80::3]:8090"));
        assert!(!fwd.allows("192.168.1.5:8090") && !fwd.allows("inspector.example:8090"));
        assert!(!lo.allows("localhost:9000"), "another port needs an entry");
        // A malformed entry is refused when the server starts.
        for bad in ["nuc.lan:abc", "[::1", "evil/x", ""] {
            let bad = vec![bad.to_string()];
            assert!(HostPolicy::new("0.0.0.0:8090".parse().unwrap(), &bad).is_err());
        }
    }

    #[test]
    fn decode_record_handles_addr1_records_and_short_payloads() {
        let head = HeadRecord {
            first_unit: 7,
            units: 8,
            last_block_hash: B256::repeat_byte(1),
            last_block_timestamp: 1,
            pending: vec![],
        };
        assert_eq!(decode_record(&head.encode())["kind"], "addrHead");
        // A descriptor, an inline and a paged bucket, built by the slice builder.
        let apps: Vec<crate::addr::detect::Appearance> = (0..2000u64)
            .map(|i| crate::addr::detect::Appearance {
                block: 64 + i % 64,
                slot: 1,
                address: Address::left_padding_from(&i.to_be_bytes()),
                roles: 1,
            })
            .collect();
        let built = crate::addr::slice::build_slice((0, 1), 0, &apps, B256::ZERO, 0x7f).unwrap();
        assert_eq!(decode_record(&built.record.encode())["kind"], "addrSlice");
        assert_eq!(decode_record(&built.buckets[0])["kind"], "addrBucket");
        assert_eq!(decode_record(&built.buckets[0])["bucket"]["paged"], false);
        let mut paged = BucketRecord::decode(&built.buckets[0], 64).unwrap();
        paged.body = BucketBody::Paged {
            count: 2,
            pages: 1,
            pages_ref: B256::repeat_byte(7),
        };
        assert_eq!(decode_record(&paged.encode())["bucket"]["paged"], true);
        // A bucket header cut short must not panic: it is simply not a record.
        for n in 5..12 {
            let mut p = b"E2S\x01\x05".to_vec();
            p.resize(n, 0);
            let _ = decode_record(&p);
        }
    }

    #[test]
    fn body_contents_from_roots() {
        assert!(
            body_contents(&header()).is_empty(),
            "pre-Shanghai empty body"
        );

        let mut h = header();
        h.withdrawals_root = Some(EMPTY_ROOT_HASH);
        assert!(
            body_contents(&h).is_empty(),
            "post-Shanghai, no withdrawals"
        );

        h.withdrawals_root = Some(keccak256(b"withdrawals"));
        assert_eq!(body_contents(&h), ["withdrawals"], "withdrawals only");

        h.transactions_root = keccak256(b"txs");
        h.ommers_hash = keccak256(b"uncles");
        assert_eq!(body_contents(&h), ["txs", "uncles", "withdrawals"]);
    }
}
