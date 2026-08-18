use std::{future::Future, io};

use tokio::net::TcpListener;

use crate::Service;

pub async fn run(config: http_support::server::Config, service: Service) -> io::Result<()> {
    let listener = TcpListener::bind(config.bind_addr()).await?;
    serve_until(listener, service, config, shutdown_signal()).await
}

pub async fn serve(
    listener: TcpListener,
    service: Service,
    config: http_support::server::Config,
) -> io::Result<()> {
    let router = service.router(&config).map_err(io::Error::other)?;
    axum::serve(listener, router).await
}

pub async fn serve_until<F>(
    listener: TcpListener,
    service: Service,
    config: http_support::server::Config,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let router = service.router(&config).map_err(io::Error::other)?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
