use std::{
    io::{Read, Write},
    time::Duration,
};

use serde_json::{Value, json};

use crate::{Call, Client, Config, Http};

#[test]
fn debug_never_exposes_endpoints_or_header_values() {
    let mut config = Config::new(
        "https://user:secret@example.invalid",
        Duration::from_secs(1),
    );
    config
        .headers
        .push(("authorization".to_owned(), "Bearer hidden".to_owned()));
    let client = Http::new(config).expect("valid HTTP configuration must build");
    let debug = format!("{client:?}");
    assert!(debug.contains("authorization"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("Bearer hidden"));
}

#[tokio::test]
async fn jsonrpsee_restores_batch_order() {
    let endpoint = serve_once(|request| {
        let calls = request.as_array().expect("batch request must be an array");
        json!([
            {"jsonrpc":"2.0", "id":calls[1]["id"], "result":"second"},
            {"jsonrpc":"2.0", "id":calls[0]["id"], "result":"first"}
        ])
    });
    let client = Http::new(Config::new(endpoint, Duration::from_secs(2)))
        .expect("loopback client must build");
    let results = client
        .batch(vec![
            Call::new("first", json!([])),
            Call::new("second", json!([])),
        ])
        .await
        .expect("batch must succeed");
    let values = results
        .into_iter()
        .map(|result| {
            result
                .expect("call must succeed")
                .deserialize::<String>()
                .expect("string must decode")
        })
        .collect::<Vec<_>>();
    assert_eq!(values, ["first", "second"]);
}

#[tokio::test]
async fn failover_advances_after_transport_failure() {
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral address must bind")
        .local_addr()
        .expect("address must exist");
    let endpoint = serve_once(|request| json!({"jsonrpc":"2.0", "id":request["id"], "result":42}));
    let mut config = Config::new(format!("http://{unavailable}"), Duration::from_secs(2));
    config.endpoints.push(endpoint);
    let client = Http::new(config).expect("failover client must build");
    let result = client
        .request("answer", json!([]))
        .await
        .expect("second endpoint must be attempted")
        .expect("call must succeed");
    assert_eq!(result.deserialize::<u64>().expect("number must decode"), 42);
}

#[tokio::test]
async fn one_attempt_does_not_hide_submission_provenance_with_failover() {
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral address must bind")
        .local_addr()
        .expect("address must exist");
    let endpoint =
        serve_once(|request| json!({"jsonrpc":"2.0", "id":request["id"], "result":"accepted"}));
    let mut config = Config::new(format!("http://{unavailable}"), Duration::from_millis(100));
    config.endpoints.push(endpoint);
    let client = Http::new(config).expect("failover client must build");

    client
        .request_once("eth_sendRawTransaction", json!(["0x02"]))
        .await
        .expect_err("one attempt must expose the first endpoint transport failure");
    let result = client
        .request("eth_sendRawTransaction", json!(["0x02"]))
        .await
        .expect("ordinary request may fail over")
        .expect("second endpoint must accept");
    assert_eq!(
        result.deserialize::<String>().expect("string must decode"),
        "accepted"
    );
}

fn serve_once(response: impl FnOnce(Value) -> Value + Send + 'static) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout must configure");
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let count = stream.read(&mut chunk).expect("request must read");
            bytes.extend_from_slice(&chunk[..count]);
            let Some(split) = bytes.windows(4).position(|value| value == b"\r\n\r\n") else {
                continue;
            };
            let header = String::from_utf8_lossy(&bytes[..split]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                })
                .expect("request must declare content length");
            let body_start = split + 4;
            if bytes.len() < body_start + length {
                continue;
            }
            let request = serde_json::from_slice(&bytes[body_start..body_start + length])
                .expect("request body must be JSON");
            let body = serde_json::to_vec(&response(request)).expect("response must encode");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("headers must write");
            stream.write_all(&body).expect("body must write");
            break;
        }
    });
    format!("http://{address}")
}
