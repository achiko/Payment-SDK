use std::{
    io::{Read, Write},
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};

use crate::{Call, Client, Config, ErrorKind, Http, Retry};

#[test]
fn config_debug_never_exposes_endpoints_or_header_values() {
    let mut config = Config::new(
        "https://user:config-secret@example.invalid",
        Duration::from_secs(1),
    );
    config
        .endpoints
        .push("https://secondary-secret.example.invalid".to_owned());
    config.headers.push((
        "authorization".to_owned(),
        "Bearer config-hidden".to_owned(),
    ));

    let debug = format!("{config:?}");
    assert!(debug.contains("endpoint_count: 2"));
    assert!(debug.contains("authorization"));
    assert!(!debug.contains("config-secret"));
    assert!(!debug.contains("secondary-secret"));
    assert!(!debug.contains("Bearer config-hidden"));
}

#[test]
fn http_debug_never_exposes_endpoints_or_header_values() {
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
async fn request_once_executes_only_the_selected_endpoint() {
    let selected_executions = Arc::new(AtomicUsize::new(0));
    let selected = serve_status_once(Arc::clone(&selected_executions), "503 Service Unavailable");
    let fallback_executions = Arc::new(AtomicUsize::new(0));
    let fallback_count = Arc::clone(&fallback_executions);
    let fallback = serve_once(move |request| {
        fallback_count.fetch_add(1, Ordering::SeqCst);
        json!({"jsonrpc":"2.0", "id":request["id"], "result":"accepted"})
    });
    let mut config = Config::new(selected, Duration::from_secs(2));
    config.endpoints.push(fallback);
    config.retry = Retry::new(
        NonZeroU32::new(3).expect("three is nonzero"),
        Duration::ZERO,
        Duration::ZERO,
    )
    .expect("zero backoff is valid");
    let client = Http::new(config).expect("failover client must build");

    let error = client
        .request_once("eth_sendRawTransaction", json!(["0x02"]))
        .await
        .expect_err("one attempt must expose the selected endpoint rejection");
    assert_eq!(error.kind, ErrorKind::HttpStatus(503));
    assert_eq!(selected_executions.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_executions.load(Ordering::SeqCst), 0);

    let result = client
        .request("eth_sendRawTransaction", json!(["0x02"]))
        .await
        .expect("ordinary request may fail over")
        .expect("second endpoint must accept");
    assert_eq!(
        result.deserialize::<String>().expect("string must decode"),
        "accepted"
    );
    assert_eq!(fallback_executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_once_can_be_cancelled_after_one_http_execution() {
    let HeldServer {
        endpoint,
        observed,
        release,
        thread,
    } = serve_held();
    let client = Http::new(Config::new(endpoint, Duration::from_secs(2)))
        .expect("loopback client must build");
    let request = tokio::spawn(async move { client.request_once("held", json!([])).await });
    let observed = tokio::task::spawn_blocking(move || {
        observed
            .recv_timeout(Duration::from_secs(2))
            .expect("one request must reach the server")
    })
    .await
    .expect("request observer must not panic");
    assert_eq!(observed["method"], "held");

    request.abort();
    let cancelled = request
        .await
        .expect_err("aborted request task must report cancellation");
    assert!(cancelled.is_cancelled());
    release
        .send(())
        .expect("held loopback response must be released");
    tokio::task::spawn_blocking(move || {
        thread.join().expect("held loopback server must not panic");
    })
    .await
    .expect("held loopback join must not panic");
}

#[tokio::test]
async fn request_once_rejects_an_oversized_response_after_one_execution() {
    let executions = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&executions);
    let endpoint = serve_once(move |request| {
        count.fetch_add(1, Ordering::SeqCst);
        json!({"jsonrpc":"2.0", "id":request["id"], "result":"x".repeat(256)})
    });
    let mut config = Config::new(endpoint, Duration::from_secs(2));
    config.max_response_bytes = 64;
    let client = Http::new(config).expect("bounded loopback client must build");

    let error = client
        .request_once("oversized", json!([]))
        .await
        .expect_err("oversized response must fail");
    assert_eq!(error.kind, ErrorKind::ResponseTooLarge);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_once_rejects_a_mismatched_wire_id() {
    let endpoint = serve_once(|request| {
        let request_id = request["id"]
            .as_u64()
            .expect("request wire ID must be an unsigned integer");
        let response_id = request_id
            .checked_add(1)
            .expect("test request ID must have a successor");
        json!({"jsonrpc":"2.0", "id":response_id, "result":"wrong"})
    });
    let client = Http::new(Config::new(endpoint, Duration::from_secs(2)))
        .expect("loopback client must build");

    let error = client
        .request_once("correlated", json!([]))
        .await
        .expect_err("response ID must match the request wire ID");
    assert_eq!(error.kind, ErrorKind::InvalidResponse);
}

fn serve_once(response: impl FnOnce(Value) -> Value + Send + 'static) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        let request = read_request(&mut stream);
        write_json_response(&mut stream, &response(request)).expect("response must write");
    });
    format!("http://{address}")
}

fn serve_status_once(executions: Arc<AtomicUsize>, status: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        let _ = read_request(&mut stream);
        executions.fetch_add(1, Ordering::SeqCst);
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .expect("status response must write");
    });
    format!("http://{address}")
}

struct HeldServer {
    endpoint: String,
    observed: mpsc::Receiver<Value>,
    release: mpsc::Sender<()>,
    thread: thread::JoinHandle<()>,
}

fn serve_held() -> HeldServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback server must bind");
    let address = listener.local_addr().expect("loopback address must exist");
    let (observed_send, observed) = mpsc::channel();
    let (release, release_receive) = mpsc::channel();
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        let request = read_request(&mut stream);
        observed_send
            .send(request.clone())
            .expect("request observation must send");
        release_receive
            .recv_timeout(Duration::from_secs(2))
            .expect("held response must be released");
        let response = json!({"jsonrpc":"2.0", "id":request["id"], "result":"released"});
        let _ = write_json_response(&mut stream, &response);
    });
    HeldServer {
        endpoint: format!("http://{address}"),
        observed,
        release,
        thread,
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout must configure");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = stream.read(&mut chunk).expect("request must read");
        assert_ne!(count, 0, "request body ended before it was complete");
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
        return serde_json::from_slice(&bytes[body_start..body_start + length])
            .expect("request body must be JSON");
    }
}

fn write_json_response(stream: &mut std::net::TcpStream, response: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(response).expect("response must encode");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)
}
