//! `devnet-activity`: traffic for a dev chain (the bee-factory Anvil), so that the address
//! index sees every kind of appearance: ETH transfers, a contract creation and one that
//! fails, token-style `Transfer` logs, a busy address whose entries spill, and an EIP-7702
//! authorization.
//! Only for dev chains: it signs with the keys it is given.

use alloy::eips::eip7702::Authorization;
use alloy::network::{EthereumWallet, TransactionBuilder, TransactionBuilder7702};
use alloy::primitives::{Address, Bytes, U256, hex};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::http::reqwest::Url;
use anyhow::{Context, Result, ensure};
use tracing::info;

/// Init code of the emitter contract: copies the 50-byte runtime below to memory and
/// returns it.
const INIT: &str = "6032600c60003960326000f3";

/// Runtime of the emitter contract. Called with a 32-byte word `to` as call data, it emits
/// `Transfer(address,address,uint256)` with topics (signature, caller, `to`) and data `to`:
///
/// ```text
/// PUSH1 32  PUSH1 0  PUSH1 0  CALLDATACOPY        ; mem[0..32] = calldata[0..32]
/// PUSH1 0  CALLDATALOAD                           ; topic2 = to
/// CALLER                                          ; topic1 = caller
/// PUSH32 keccak256("Transfer(address,address,uint256)")
/// PUSH1 32  PUSH1 0  LOG3  STOP                   ; data = mem[0..32]
/// ```
const RUNTIME: &str = concat!(
    "6020600060003760003533",
    "7f",
    "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
    "60206000a300"
);

/// What `devnet-activity` sent.
pub struct Activity {
    pub emitter: Address,
    /// The address that receives every `Transfer`: enough entries to spill.
    pub busy: Address,
    pub first_block: u64,
    pub last_block: u64,
    pub transactions: usize,
    /// The authority of the EIP-7702 authorization, if the chain accepted it.
    pub authority: Option<Address>,
    /// The target of a top-level creation that reverts: `TO`, and with traces `TRACE` (the
    /// `TRACE` rule counts a creation's target whatever its outcome).
    pub failed_creation: Address,
}

/// Chain IDs of local dev chains (Anvil, Hardhat, bee-factory). Anything else needs an
/// explicit `allow_chain_id`.
pub const DEV_CHAINS: [u64; 2] = [1337, 31337];

/// Sends the activity from `sender`; `authority` signs an EIP-7702 authorization that
/// `sender` submits. `transfers` is the number of `Transfer` calls.
pub async fn run(
    rpc: Url,
    sender: PrivateKeySigner,
    authority: PrivateKeySigner,
    busy: Address,
    transfers: usize,
    allow_chain_id: Option<u64>,
) -> Result<Activity> {
    let from = sender.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(sender))
        .connect_http(rpc);
    let chain_id = provider.get_chain_id().await?;
    // This sends real transactions and leaves an EIP-7702 delegation on the authority's
    // account: only on a dev chain, or on the one chain the caller names.
    ensure!(
        DEV_CHAINS.contains(&chain_id) || allow_chain_id == Some(chain_id),
        "chain {chain_id} is not a dev chain ({DEV_CHAINS:?}); devnet-activity sends \
         transactions with the given keys. Pass --allow-chain-id {chain_id} if that is intended."
    );
    let mut nonce = provider
        .get_transaction_count(from)
        .pending()
        .await
        .context("reading the sender's nonce")?;
    let tx = |nonce: u64| {
        TransactionRequest::default()
            .with_from(from)
            .with_nonce(nonce)
    };

    // 1. Deploy the emitter.
    let code: Bytes = hex::decode(format!("{INIT}{RUNTIME}"))?.into();
    let receipt = provider
        .send_transaction(tx(nonce).with_deploy_code(code))
        .await?
        .get_receipt()
        .await?;
    nonce += 1;
    ensure!(receipt.status(), "deploying the emitter failed");
    let emitter = receipt
        .contract_address
        .context("no contract address in the deployment receipt")?;
    let first_block = receipt
        .block_number
        .context("receipt without a block number")?;
    info!(%emitter, "emitter deployed");

    // 2. Many transfers to the busy address, sent at once so that they share few blocks.
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(busy.as_slice());
    let mut pending = Vec::with_capacity(transfers);
    for _ in 0..transfers {
        let req = tx(nonce)
            .with_to(emitter)
            .with_input(Bytes::copy_from_slice(&word))
            .with_gas_limit(60_000);
        pending.push(provider.send_transaction(req).await?);
        nonce += 1;
    }
    let mut last_block = first_block;
    for p in pending {
        let r = p.get_receipt().await?;
        ensure!(r.status(), "a Transfer call failed");
        last_block = last_block.max(r.block_number.context("receipt without a block number")?);
    }

    // 3. Plain ETH transfers to the busy address.
    for _ in 0..3 {
        let r = provider
            .send_transaction(tx(nonce).with_to(busy).with_value(U256::from(1_000)))
            .await?
            .get_receipt()
            .await?;
        nonce += 1;
        last_block = last_block.max(r.block_number.context("receipt without a block number")?);
    }

    // 4. An EIP-7702 authorization by another key, delegating to the emitter.
    let auth_nonce = provider.get_transaction_count(authority.address()).await?;
    let auth = Authorization {
        chain_id: U256::from(chain_id),
        address: emitter,
        nonce: auth_nonce,
    };
    let sig = authority.sign_hash_sync(&auth.signature_hash())?;
    let req = tx(nonce)
        .with_to(busy)
        .with_authorization_list(vec![auth.into_signed(sig)])
        .with_gas_limit(100_000);
    let authority_used = match provider.send_transaction(req).await {
        Ok(p) => {
            let r = p.get_receipt().await?;
            nonce += 1;
            last_block = last_block.max(r.block_number.context("receipt without a block number")?);
            Some(authority.address())
        }
        Err(e) => {
            tracing::warn!("the chain did not take an EIP-7702 transaction: {e:#}");
            None
        }
    };

    // 5. A creation whose init code reverts (PUSH1 0, PUSH1 0, REVERT). An explicit gas
    // limit: estimating a reverting transaction fails.
    let failed_creation = from.create(nonce);
    let r = provider
        .send_transaction(
            tx(nonce)
                .with_deploy_code(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]))
                .with_gas_limit(100_000),
        )
        .await?
        .get_receipt()
        .await?;
    ensure!(!r.status(), "the reverting creation succeeded");
    last_block = last_block.max(r.block_number.context("receipt without a block number")?);

    Ok(Activity {
        emitter,
        busy,
        first_block,
        last_block,
        transactions: transfers + 5 + usize::from(authority_used.is_some()),
        authority: authority_used,
        failed_creation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The init code copies `RUNTIME` from right after itself and returns it: its length and
    /// offset must match.
    #[test]
    fn init_code_matches_runtime() {
        let init = hex::decode(INIT).unwrap();
        let runtime = hex::decode(RUNTIME).unwrap();
        // PUSH1 len (size of CODECOPY), PUSH1 offset, …, PUSH1 len (size of RETURN), …
        assert_eq!(init[1] as usize, runtime.len());
        assert_eq!(init[3] as usize, init.len());
        assert_eq!(init[8] as usize, runtime.len());
        // The topic is Transfer(address,address,uint256).
        assert_eq!(
            &runtime[12..44],
            alloy::primitives::keccak256("Transfer(address,address,uint256)").as_slice()
        );
    }
}
