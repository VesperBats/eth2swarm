//! Appearance detection: which addresses occur in a block, where, and in what role.
//!
//! The rules are normative (`docs/address-index.md`, "Detection rules"): independent
//! importers must find exactly the same appearances. They are a superset of TrueBlocks'
//! (`trueblocks-chifra`, `pkg/uniq`): the same trace, receipt, withdrawal and miner sources
//! with the same address-shaped-word heuristic, plus log emitters, known token events
//! regardless of value, and EIP-7702 authorities.
//!
//! [`detect`] is pure: [`BlockData`] holds what the RPC delivered (see `super::fetch`).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};

use alloy::eips::eip7702::SignedAuthorization;
use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use serde::Deserialize;

/// Role bits of an appearance.
pub mod role {
    /// Sender of the transaction.
    pub const FROM: u8 = 0x01;
    /// Recipient, or the contract the transaction created.
    pub const TO: u8 = 0x02;
    /// Address of a log the transaction emitted.
    pub const EMITTER: u8 = 0x04;
    /// Address in a log topic.
    pub const TOPIC: u8 = 0x08;
    /// Address-shaped 32-byte word in a transaction's input or init code, in log data, or an
    /// EIP-7702 code address (traces add no words).
    pub const WORD: u8 = 0x10;
    /// Address of a call frame (the top-level one included), creation or self-destruct (from
    /// traces).
    pub const TRACE: u8 = 0x20;
    /// Block level: fee recipient / miner, uncle miner, reward author, withdrawal recipient.
    pub const BLOCK: u8 = 0x40;
    /// Authority of an EIP-7702 authorization.
    pub const AUTH: u8 = 0x80;

    /// Role names, lowest bit first.
    pub const NAMES: [(u8, &str); 8] = [
        (FROM, "from"),
        (TO, "to"),
        (EMITTER, "emitter"),
        (TOPIC, "topic"),
        (WORD, "word"),
        (TRACE, "trace"),
        (BLOCK, "block"),
        (AUTH, "auth"),
    ];

    /// The roles an importer detects: all of them with traces, all but `TRACE` without.
    pub fn mask(traces: bool) -> u8 {
        if traces { 0xff } else { !TRACE }
    }

    /// `"from|topic"` style rendering.
    pub fn names(roles: u8) -> String {
        NAMES
            .iter()
            .filter(|(bit, _)| roles & bit != 0)
            .map(|(_, name)| *name)
            .collect::<Vec<_>>()
            .join("|")
    }
}

/// One address at one (block, slot), with the OR of all roles it has there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Appearance {
    pub block: u64,
    /// `0` for block-level appearances, `txIndex + 1` for a transaction.
    pub slot: u32,
    pub address: Address,
    pub roles: u8,
}

/// What detection needs from one block.
#[derive(Debug, Clone, Default)]
pub struct BlockData {
    pub number: u64,
    /// Block hash (verified by `fetch`).
    pub hash: B256,
    /// Parent block hash, from the verified header.
    pub parent_hash: B256,
    /// Block timestamp (seconds), from the verified header.
    pub timestamp: u64,
    /// Header beneficiary (fee recipient, miner).
    pub beneficiary: Address,
    pub uncle_beneficiaries: Vec<Address>,
    pub withdrawal_recipients: Vec<Address>,
    /// Transactions in block order, each with its receipt's logs.
    pub txs: Vec<TxData>,
    /// Parity-style `trace_block` output, if the source has traces.
    pub traces: Option<Vec<Trace>>,
}

#[derive(Debug, Clone, Default)]
pub struct TxData {
    pub sender: Address,
    /// `None` for a contract creation.
    pub to: Option<Address>,
    pub nonce: u64,
    /// Call data, or init code for a creation.
    pub input: Bytes,
    pub authorizations: Vec<SignedAuthorization>,
    pub logs: Vec<LogData>,
}

#[derive(Debug, Clone, Default)]
pub struct LogData {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

/// One entry of a Parity-style `trace_block` response (only the fields detection reads).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trace {
    #[serde(default)]
    pub action: TraceAction,
    #[serde(default)]
    pub result: Option<TraceResult>,
    /// Position of the transaction in the block; absent for block rewards.
    #[serde(default)]
    pub transaction_position: Option<u64>,
    #[serde(default)]
    pub transaction_hash: Option<B256>,
    #[serde(default)]
    pub block_hash: Option<B256>,
    /// Path of the call frame below the transaction's root frame (`[]` for the root).
    #[serde(default)]
    pub trace_address: Vec<u64>,
    /// `call`, `create`, `suicide`, `reward`.
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceAction {
    pub from: Option<Address>,
    pub to: Option<Address>,
    /// Self-destructed contract.
    pub address: Option<Address>,
    /// Beneficiary of a self-destruct.
    pub refund_address: Option<Address>,
    /// Recipient of a block or uncle reward.
    pub author: Option<Address>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceResult {
    /// Created contract.
    pub address: Option<Address>,
}

/// Trace types detection understands. A block whose traces contain another type is
/// rejected (see [`check_traces`]), so that an importer never silently disagrees with one
/// that knows the type.
pub const TRACE_KINDS: [&str; 4] = ["call", "create", "suicide", "reward"];

/// Checks that `traces` are a complete `trace_block` answer for the verified block with hash
/// `block_hash`, transactions `tx_hashes` and beneficiaries `beneficiaries` (header and
/// uncles). Traces cannot be verified without re-execution; this rules out traces of another
/// block, a partial or empty answer, and fields detection depends on being absent:
///
/// - every trace has a known type and this block's hash;
/// - a `reward` names no transaction, and its author is the beneficiary of the block or of
///   an uncle;
/// - every other trace names an existing transaction by position and hash;
/// - every transaction has exactly one root trace (empty `traceAddress`).
pub fn check_traces(
    traces: &[Trace],
    block_hash: B256,
    tx_hashes: &[B256],
    beneficiaries: &[Address],
) -> Result<()> {
    let mut roots = vec![0usize; tx_hashes.len()];
    for (k, t) in traces.iter().enumerate() {
        if !TRACE_KINDS.contains(&t.kind.as_str()) {
            bail!("trace {k} has unknown type {:?}", t.kind);
        }
        let h = t
            .block_hash
            .with_context(|| format!("trace {k} has no blockHash"))?;
        ensure!(
            h == block_hash,
            "trace {k} is for block {h}, not {block_hash}"
        );
        if t.kind == "reward" {
            ensure!(
                t.transaction_position.is_none(),
                "reward trace {k} names a transaction"
            );
            let author = t
                .action
                .author
                .with_context(|| format!("reward trace {k} has no author"))?;
            ensure!(
                beneficiaries.contains(&author),
                "reward trace {k}: author {author} is not a beneficiary of the block or an uncle"
            );
            continue;
        }
        let pos = t
            .transaction_position
            .with_context(|| format!("{} trace {k} names no transaction", t.kind))?;
        let expected = tx_hashes
            .get(pos as usize)
            .with_context(|| format!("trace {k} names transaction {pos} of {}", tx_hashes.len()))?;
        let h = t
            .transaction_hash
            .with_context(|| format!("trace {k} has no transactionHash"))?;
        ensure!(
            h == *expected,
            "trace {k} names transaction {pos} with hash {h}, expected {expected}"
        );
        if t.trace_address.is_empty() {
            roots[pos as usize] += 1;
        }
    }
    if let Some(pos) = roots.iter().position(|n| *n != 1) {
        bail!(
            "transaction {pos} has {} root traces, expected 1 (incomplete trace_block answer?)",
            roots[pos]
        );
    }
    Ok(())
}

/// Addresses up to `0x…ffff` (the zero address, precompiles) are never indexed, as in
/// TrueBlocks.
pub fn ignored(address: &Address) -> bool {
    address.0[..18].iter().all(|b| *b == 0)
}

/// TrueBlocks' "implicit address" test for a 32-byte word: 12 zero bytes, then a value of
/// at least 2^104 (one of bytes 12–18 non-zero), and not ending in four zero bytes (round
/// amounts).
pub fn word_address(word: &[u8]) -> Option<Address> {
    if word.len() != 32 || word[..12].iter().any(|b| *b != 0) {
        return None;
    }
    if word[12..19].iter().all(|b| *b == 0) || word[28..].iter().all(|b| *b == 0) {
        return None;
    }
    Some(Address::from_slice(&word[12..]))
}

/// Address-shaped words of `data` at byte offsets `skip + 32·k`.
fn words(data: &[u8], skip: usize) -> impl Iterator<Item = Address> + '_ {
    data.get(skip..)
        .unwrap_or_default()
        .as_chunks::<32>()
        .0
        .iter()
        .filter_map(|w| word_address(w))
}

/// keccak256 of the event signatures whose listed topics are addresses whatever their value.
fn known_event_topics(topic0: &B256) -> &'static [usize] {
    use std::sync::OnceLock;
    static KNOWN: OnceLock<Vec<(B256, &'static [usize])>> = OnceLock::new();
    let known = KNOWN.get_or_init(|| {
        [
            ("Transfer(address,address,uint256)", &[1usize, 2][..]),
            ("Approval(address,address,uint256)", &[1, 2]),
            ("ApprovalForAll(address,address,bool)", &[1, 2]),
            (
                "TransferSingle(address,address,address,uint256,uint256)",
                &[1, 2, 3],
            ),
            (
                "TransferBatch(address,address,address,uint256[],uint256[])",
                &[1, 2, 3],
            ),
            ("Deposit(address,uint256)", &[1]),
            ("Withdrawal(address,uint256)", &[1]),
        ]
        .into_iter()
        .map(|(sig, idx)| (keccak256(sig), idx))
        .collect()
    });
    known
        .iter()
        .find(|(t, _)| t == topic0)
        .map_or(&[], |(_, idx)| idx)
}

/// secp256k1 group order `n`.
const SECP256K1_N: U256 = U256::from_be_slice(&[
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
]);

/// The authority of an EIP-7702 authorization entry, if the entry counts: `y_parity ∈ {0, 1}`,
/// `0 < r < n`, `0 < s ≤ n/2`, and recovery succeeds. Chain ID and nonce are not checked.
/// `recover_authority` checks the parity and `s ≤ n/2`; the range checks it leaves out are
/// done here.
pub fn authority(auth: &SignedAuthorization) -> Option<Address> {
    let (r, s) = (auth.r(), auth.s());
    if r.is_zero() || r >= SECP256K1_N || s.is_zero() {
        return None;
    }
    auth.recover_authority().ok()
}

/// Collects appearances, merging roles per (address, slot).
#[derive(Default)]
struct Collector {
    block: u64,
    seen: BTreeMap<(u32, Address), u8>,
}

impl Collector {
    fn add(&mut self, address: Address, slot: u32, roles: u8) {
        if !ignored(&address) {
            *self.seen.entry((slot, address)).or_default() |= roles;
        }
    }

    fn finish(self) -> Vec<Appearance> {
        let block = self.block;
        self.seen
            .into_iter()
            .map(|((slot, address), roles)| Appearance {
                block,
                slot,
                address,
                roles,
            })
            .collect()
    }
}

/// All appearances in a block, ascending by (slot, address). The traces, if any, must have
/// passed [`check_traces`].
pub fn detect(block: &BlockData) -> Vec<Appearance> {
    use role::*;
    let mut c = Collector {
        block: block.number,
        ..Default::default()
    };

    c.add(block.beneficiary, 0, BLOCK);
    for a in &block.uncle_beneficiaries {
        c.add(*a, 0, BLOCK);
    }
    for a in &block.withdrawal_recipients {
        c.add(*a, 0, BLOCK);
    }

    for (i, tx) in block.txs.iter().enumerate() {
        let slot = i as u32 + 1;
        c.add(tx.sender, slot, FROM);
        let to = tx.to.unwrap_or_else(|| tx.sender.create(tx.nonce));
        c.add(to, slot, TO);
        // Call data after the selector; for a creation the init code, scanned the same way
        // (as TrueBlocks does for top-level creations).
        for a in words(&tx.input, 4) {
            c.add(a, slot, WORD);
        }
        for auth in &tx.authorizations {
            if let Some(a) = authority(auth) {
                c.add(a, slot, AUTH);
            }
            c.add(auth.address, slot, WORD);
        }
        for log in &tx.logs {
            c.add(log.address, slot, EMITTER);
            let known = log.topics.first().map_or(&[][..], known_event_topics);
            for (t, topic) in log.topics.iter().enumerate() {
                if known.contains(&t) {
                    if topic[..12].iter().all(|b| *b == 0) {
                        c.add(Address::from_slice(&topic[12..]), slot, TOPIC);
                    }
                } else if let Some(a) = word_address(topic.as_slice()) {
                    c.add(a, slot, TOPIC);
                }
            }
            for a in words(&log.data, 0) {
                c.add(a, slot, WORD);
            }
        }
    }

    for trace in block.traces.iter().flatten() {
        let act = &trace.action;
        match (trace.kind.as_str(), trace.transaction_position) {
            // Block and uncle rewards (pre-merge).
            ("reward", _) => {
                if let Some(a) = act.author {
                    c.add(a, 0, BLOCK);
                }
            }
            ("call" | "create" | "suicide", Some(pos)) => {
                let slot = pos as u32 + 1;
                // Call: from/to of the frame. Create: the creator. Suicide: the destroyed
                // contract and its beneficiary.
                for a in [act.from, act.to, act.address, act.refund_address]
                    .into_iter()
                    .flatten()
                {
                    c.add(a, slot, TRACE);
                }
                if let Some(res) = &trace.result
                    && trace.kind == "create"
                    && let Some(a) = res.address
                {
                    c.add(a, slot, TRACE);
                }
                // The input and output of internal calls are not scanned for words: traces
                // add only the frames' own addresses, the set an archive node indexes.
            }
            // Other types, or a transaction trace without a position: nothing. Callers run
            // [`check_traces`] first, which rejects both.
            _ => {}
        }
    }

    c.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::eips::eip7702::Authorization;
    use alloy::primitives::{address, b256, hex};
    use alloy::signers::SignerSync;
    use alloy::signers::local::PrivateKeySigner;

    fn word(hex32: &str) -> Vec<u8> {
        hex::decode(hex32).unwrap()
    }

    // TrueBlocks' own vectors (pkg/uniq/address_isimplicit_test.go).
    #[test]
    fn word_heuristic_matches_trueblocks() {
        let pad = "000000000000000000000000";
        let valid = "f1aa581f353005ba3765b81bf52d6b1c488c2101";
        assert_eq!(
            word_address(&word(&format!("{pad}{valid}"))),
            Some(Address::from_slice(&hex::decode(valid).unwrap()))
        );
        // Too small: at most 13 significant bytes.
        let too_small = "00000000000000000000000000000000000000ffffffffffffffffffffffffff";
        assert_eq!(word_address(&word(too_small)), None);
        // One more significant byte is enough.
        let just_big = "0000000000000000000000000000000000000100000000000000000000000001";
        assert!(word_address(&word(just_big)).is_some());
        // Not left-padded with 12 zero bytes.
        assert_eq!(
            word_address(&word(&format!("abcdefabcdefabcdefabcdef{valid}"))),
            None
        );
        // Trailing four zero bytes: a round amount.
        assert_eq!(
            word_address(&word(&format!(
                "{pad}1111111111111111111111111111111100000000"
            ))),
            None
        );
        assert_eq!(word_address(&[0u8; 2]), None);
        // Permit2 (five leading zero bytes) passes; the doc's earlier 2^128 bound did not.
        let permit2 = address!("0x000000000022D473030F116dDEE9F6B43aC78BA3");
        let mut w = [0u8; 32];
        w[12..].copy_from_slice(permit2.as_slice());
        assert_eq!(word_address(&w), Some(permit2));
    }

    #[test]
    fn ignored_up_to_0xffff() {
        assert!(ignored(&Address::ZERO));
        assert!(ignored(&address!(
            "0x0000000000000000000000000000000000000001"
        )));
        assert!(ignored(&address!(
            "0x000000000000000000000000000000000000ffff"
        )));
        assert!(!ignored(&address!(
            "0x0000000000000000000000000000000000010000"
        )));
    }

    fn padded(a: Address) -> B256 {
        let mut w = B256::ZERO;
        w[12..].copy_from_slice(a.as_slice());
        w
    }

    fn find(apps: &[Appearance], a: Address, slot: u32) -> Option<u8> {
        apps.iter()
            .find(|x| x.address == a && x.slot == slot)
            .map(|x| x.roles)
    }

    const A: Address = address!("0x1111111111111111111111111111111111111111");
    const B: Address = address!("0x2222222222222222222222222222222222222222");
    const TOKEN: Address = address!("0x3333333333333333333333333333333333333333");

    #[test]
    fn block_level_and_transfer() {
        let vanity = address!("0x00000000000000ADc04C56Bf30aC9d3c0aAF14dC"); // 7 zero bytes
        let block = BlockData {
            number: 7,
            beneficiary: B,
            withdrawal_recipients: vec![A, Address::ZERO],
            txs: vec![TxData {
                sender: A,
                to: Some(TOKEN),
                nonce: 3,
                input: Bytes::from([&[0xa9, 0x05, 0x9c, 0xbb][..], padded(B).as_slice()].concat()),
                logs: vec![LogData {
                    address: TOKEN,
                    topics: vec![
                        keccak256("Transfer(address,address,uint256)"),
                        padded(A),
                        padded(vanity),
                    ],
                    data: Bytes::from(U256::from(10).to_be_bytes::<32>().to_vec()),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let apps = detect(&block);
        use role::*;
        assert_eq!(find(&apps, B, 0), Some(BLOCK));
        assert_eq!(find(&apps, A, 0), Some(BLOCK));
        assert_eq!(find(&apps, Address::ZERO, 0), None);
        assert_eq!(find(&apps, A, 1), Some(FROM | TOPIC));
        assert_eq!(find(&apps, TOKEN, 1), Some(TO | EMITTER));
        assert_eq!(find(&apps, B, 1), Some(WORD));
        // A known event's address topic counts whatever its value.
        assert_eq!(find(&apps, vanity, 1), Some(TOPIC));
        assert!(apps.iter().all(|x| x.block == 7));
        let mut sorted = apps.clone();
        sorted.sort();
        assert_eq!(apps, sorted);
    }

    #[test]
    fn creation_address_from_sender_and_nonce() {
        // The classic example: sender 0x6ac7…dbf0, nonce 0.
        let sender = address!("0x6ac7ea33f8831ea9dcc53393aaa88b25a785dbf0");
        let block = BlockData {
            number: 1,
            txs: vec![TxData {
                sender,
                to: None,
                nonce: 0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let apps = detect(&block);
        assert_eq!(
            find(
                &apps,
                address!("0xcd234a471b72ba2f1ccf0a70fcaba648a5eecd8d"),
                1
            ),
            Some(role::TO)
        );
    }

    fn signed_auth(key: &PrivateKeySigner, target: Address) -> SignedAuthorization {
        let auth = Authorization {
            chain_id: U256::from(1),
            address: target,
            nonce: 5,
        };
        let sig = key.sign_hash_sync(&auth.signature_hash()).unwrap();
        auth.into_signed(sig)
    }

    #[test]
    fn eip7702_authority_rule() {
        let key = PrivateKeySigner::random();
        let ok = signed_auth(&key, TOKEN);
        assert_eq!(authority(&ok), Some(key.address()));

        let inner = ok.inner().clone();
        let high_s = SignedAuthorization::new_unchecked(
            inner.clone(),
            ok.y_parity() ^ 1,
            ok.r(),
            SECP256K1_N - ok.s(),
        );
        assert_eq!(authority(&high_s), None, "s above n/2");
        let bad_parity = SignedAuthorization::new_unchecked(inner.clone(), 2, ok.r(), ok.s());
        assert_eq!(authority(&bad_parity), None, "y_parity 2");
        let zero_r = SignedAuthorization::new_unchecked(inner, ok.y_parity(), U256::ZERO, ok.s());
        assert_eq!(authority(&zero_r), None, "r = 0");

        let block = BlockData {
            number: 9,
            txs: vec![TxData {
                sender: A,
                to: Some(A),
                authorizations: vec![ok, high_s],
                ..Default::default()
            }],
            ..Default::default()
        };
        let apps = detect(&block);
        assert_eq!(find(&apps, key.address(), 1), Some(role::AUTH));
        assert_eq!(find(&apps, TOKEN, 1), Some(role::WORD));
    }

    #[test]
    fn traces_from_parity_json() {
        let json = r#"[
          {"action":{"from":"0x1111111111111111111111111111111111111111","to":"0x3333333333333333333333333333333333333333","input":"0xa9059cbb","callType":"call","gas":"0x0","value":"0x0"},
           "result":{"gasUsed":"0x0","output":"0x"},"traceAddress":[],"transactionPosition":0,"type":"call"},
          {"action":{"from":"0x3333333333333333333333333333333333333333","to":"0x4444444444444444444444444444444444444444","input":"0x12345678000000000000000000000000555555555555555555555555555555555555555500","callType":"delegatecall"},
           "result":{"output":"0x0000000000000000000000006666666666666666666666666666666666666666"},"traceAddress":[0],"transactionPosition":0,"type":"call"},
          {"action":{"address":"0x4444444444444444444444444444444444444444","refundAddress":"0x7777777777777777777777777777777777777777","balance":"0x1"},
           "result":null,"traceAddress":[1],"transactionPosition":0,"type":"suicide"},
          {"action":{"from":"0x3333333333333333333333333333333333333333","init":"0x60","value":"0x0"},
           "result":{"address":"0x8888888888888888888888888888888888888888","code":"0x","gasUsed":"0x0"},"traceAddress":[2],"transactionPosition":0,"type":"create"},
          {"action":{"author":"0x9999999999999999999999999999999999999999","rewardType":"block","value":"0x1"},
           "result":null,"traceAddress":[],"type":"reward"}
        ]"#;
        let traces: Vec<Trace> = serde_json::from_str(json).unwrap();
        let block = BlockData {
            number: 3,
            txs: vec![TxData {
                sender: A,
                to: Some(TOKEN),
                ..Default::default()
            }],
            traces: Some(traces),
            ..Default::default()
        };
        let apps = detect(&block);
        use role::*;
        let x = |h: &str| Address::from_slice(&hex::decode(h).unwrap());
        assert_eq!(find(&apps, A, 1), Some(FROM | TRACE));
        assert_eq!(find(&apps, TOKEN, 1), Some(TO | TRACE));
        assert_eq!(
            find(&apps, x("4444444444444444444444444444444444444444"), 1),
            Some(TRACE)
        );
        // Words in an internal call's input and output are not indexed: traces add only the
        // frames' own addresses.
        assert_eq!(
            find(&apps, x("5555555555555555555555555555555555555555"), 1),
            None
        );
        assert_eq!(
            find(&apps, x("6666666666666666666666666666666666666666"), 1),
            None
        );
        // Self-destruct beneficiary, internal creation, reward author.
        assert_eq!(
            find(&apps, x("7777777777777777777777777777777777777777"), 1),
            Some(TRACE)
        );
        assert_eq!(
            find(&apps, x("8888888888888888888888888888888888888888"), 1),
            Some(TRACE)
        );
        assert_eq!(
            find(&apps, x("9999999999999999999999999999999999999999"), 0),
            Some(BLOCK)
        );
    }

    fn root_call(pos: u64, tx: B256, block: B256) -> Trace {
        Trace {
            kind: "call".into(),
            transaction_position: Some(pos),
            transaction_hash: Some(tx),
            block_hash: Some(block),
            ..Default::default()
        }
    }

    #[test]
    fn trace_checks() {
        let block = B256::repeat_byte(1);
        let txs = [B256::repeat_byte(2), B256::repeat_byte(3)];
        let miner = [B];
        let good = vec![
            root_call(0, txs[0], block),
            Trace {
                trace_address: vec![0],
                ..root_call(0, txs[0], block)
            },
            root_call(1, txs[1], block),
            Trace {
                kind: "reward".into(),
                transaction_position: None,
                transaction_hash: None,
                action: TraceAction {
                    author: Some(B),
                    ..Default::default()
                },
                ..root_call(0, txs[0], block)
            },
        ];
        check_traces(&good, block, &txs, &miner).unwrap();

        let fails = |traces: Vec<Trace>, what: &str| {
            let err = check_traces(&traces, block, &txs, &miner).unwrap_err();
            assert!(err.to_string().contains(what), "{what}: got {err}");
        };
        fails(vec![], "root traces");
        fails(good[..2].to_vec(), "transaction 1 has 0 root traces");
        let mut two_roots = good.clone();
        two_roots.push(root_call(1, txs[1], block));
        fails(two_roots, "transaction 1 has 2 root traces");
        let mut other_block = good.clone();
        other_block[2].block_hash = Some(B256::repeat_byte(9));
        fails(other_block, "is for block");
        let mut no_block_hash = good.clone();
        no_block_hash[2].block_hash = None;
        fails(no_block_hash, "no blockHash");
        let mut no_tx_hash = good.clone();
        no_tx_hash[2].transaction_hash = None;
        fails(no_tx_hash, "no transactionHash");
        let mut wrong_tx = good.clone();
        wrong_tx[2].transaction_hash = Some(txs[0]);
        fails(wrong_tx, "expected");
        let mut no_pos = good.clone();
        no_pos[2].transaction_position = None;
        fails(no_pos, "names no transaction");
        let mut out_of_range = good.clone();
        out_of_range[2].transaction_position = Some(5);
        fails(out_of_range, "transaction 5 of 2");
        let mut unknown = good.clone();
        unknown[1].kind = "selfdestruct".into();
        fails(unknown, "unknown type");
        let mut stranger = good.clone();
        stranger[3].action.author = Some(A);
        fails(stranger, "not a beneficiary");
    }

    #[test]
    fn role_names() {
        assert_eq!(role::names(role::FROM | role::WORD), "from|word");
        assert_eq!(role::names(0), "");
    }

    #[test]
    fn heuristic_topic_and_topic0() {
        // An unknown event: every topic, topic0 included, goes through the heuristic.
        let anon = b256!("0x0000000000000000000000001111111111111111111111111111111111111111");
        let block = BlockData {
            number: 1,
            txs: vec![TxData {
                sender: B,
                to: Some(TOKEN),
                logs: vec![LogData {
                    address: TOKEN,
                    topics: vec![anon],
                    data: Bytes::new(),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(find(&detect(&block), A, 1), Some(role::TOPIC));
    }
}
