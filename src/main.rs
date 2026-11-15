use alloy::providers::{Provider, ProviderBuilder};
use alloy::transports::http::reqwest::Url;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// ethswarm — import Ethereum blocks into Ethereum Swarm.
#[derive(Parser)]
#[command(name = "ethswarm", version, about)]
struct Cli {
    /// JSON-RPC endpoint of the execution client.
    /// Defaults to $ETH2SWARM_RPC, then http://localhost:8545
    /// (the bee-factory Anvil node).
    #[arg(long, global = true, env = "ETH2SWARM_RPC", default_value = "http://localhost:8545")]
    rpc: Url,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the connected chain and its current head.
    Tip,
    /// Print a summary of a single block.
    Block {
        /// Block number.
        number: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let provider = ProviderBuilder::new().connect_http(cli.rpc.clone());

    match cli.command {
        Command::Tip => {
            let chain_id = provider.get_chain_id().await.context("querying chain id")?;
            let block = provider
                .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
                .await
                .context("querying latest block")?
                .context("chain returned no latest block")?;
            println!("RPC:        {}", cli.rpc);
            println!("Chain ID:   {chain_id}");
            println!("Tip:        #{}", block.header.number);
            println!("Block hash: {}", block.header.hash);
            println!("Timestamp:  {}", block.header.timestamp);
        }
        Command::Block { number } => {
            let block = provider
                .get_block_by_number(number.into())
                .await
                .with_context(|| format!("querying block {number}"))?
                .with_context(|| format!("block {number} not found"))?;
            println!("RPC:        {}", cli.rpc);
            println!("Number:     #{}", block.header.number);
            println!("Hash:       {}", block.header.hash);
            println!("Parent:     {}", block.header.parent_hash);
            println!("Timestamp:  {}", block.header.timestamp);
            println!("Txs:        {}", block.transactions.len());
            println!("Gas used:   {}", block.header.gas_used);
        }
    }
    Ok(())
}
