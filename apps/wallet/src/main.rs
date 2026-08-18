use wallet_worker::{Config, ENV_KEYS};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let values = ENV_KEYS.iter().filter_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| ((*name).to_owned(), value))
    });
    let (server, service) = wallet_worker::compose(Config::from_variables(values)?).await?;
    wallet_worker::run(server, service).await?;
    Ok(())
}
