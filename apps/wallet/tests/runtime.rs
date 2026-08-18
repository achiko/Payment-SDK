use std::time::Duration;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
use wallet_worker::{LIVE_PATH, READY_PATH, Service, serve_until};

fn config(address: std::net::SocketAddr) -> http_support::server::Config {
    http_support::server::Config::new(
        address,
        http_support::server::TransportSecurity::PlaintextLoopback,
        Some(http_support::server::BearerToken::new("runtime-secret").expect("token")),
        http_support::server::RequestLimits::default(),
    )
}

async fn status(address: std::net::SocketAddr, path: &str, token: Option<&str>) -> u16 {
    let mut stream = TcpStream::connect(address).await.expect("connect");
    let authorization = token
        .map(|value| format!("Authorization: Bearer {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\n{authorization}Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    String::from_utf8(response)
        .expect("HTTP text")
        .split_whitespace()
        .nth(1)
        .expect("status")
        .parse()
        .expect("numeric status")
}

#[tokio::test]
async fn serves_public_health_and_protects_wallet_routes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(serve_until(
        listener,
        Service::new(),
        config(address),
        async move {
            let _ = stopped.await;
        },
    ));
    assert_eq!(status(address, LIVE_PATH, None).await, 204);
    assert_eq!(status(address, READY_PATH, None).await, 503);
    assert_eq!(status(address, "/v1/wallets/missing", None).await, 401);
    assert_eq!(
        status(address, "/v1/wallets/missing", Some("wrong")).await,
        401
    );
    assert_eq!(
        status(address, "/v1/wallets/missing", Some("runtime-secret")).await,
        404
    );
    stop.send(()).expect("stop");
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("shutdown")
        .expect("join")
        .expect("serve");
}
