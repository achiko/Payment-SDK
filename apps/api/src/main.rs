use std::{env, error::Error};

use payment_api::{Runtime, RuntimeConfig, Secrets, WalletConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let path = env::args_os().nth(1).ok_or(
        "usage: payment-api <config.json>; private keys are read from the configured environment variables",
    )?;
    let bytes = tokio::fs::read(path).await?;
    let config: RuntimeConfig = serde_json::from_slice(&bytes)?;
    let mut secrets = Secrets::new();
    secrets.insert(
        &config.server.bearer_token_env,
        env::var(&config.server.bearer_token_env)?,
    );
    if let Some(name) = &config.indexer.bearer_token_env {
        secrets.insert(name, env::var(name)?);
    }
    for wallet in &config.wallets {
        let name = match wallet {
            WalletConfig::Bitcoin(value) => &value.secret_env,
            WalletConfig::Ethereum(value) => &value.secret_env,
        };
        secrets.insert(name, env::var(name)?);
    }
    if let Some(deposits) = &config.deposits {
        for key in &deposits.keys {
            secrets.insert(&key.secret_env, env::var(&key.secret_env)?);
        }
    }
    Runtime::build(config, secrets).await?.run().await?;
    Ok(())
}
