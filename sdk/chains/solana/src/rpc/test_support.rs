use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use json_rpc::{BoxFuture, Call, CallResult, Error, ErrorKind, RawJson};
use serde_json::Value;

#[derive(Clone)]
pub struct Scripted(Arc<Mutex<VecDeque<Expected>>>);

struct Expected {
    method: &'static str,
    params: Value,
    result: Value,
}

impl Scripted {
    pub fn new<const N: usize>(values: [(&'static str, Value, Value); N]) -> Self {
        Self(Arc::new(Mutex::new(
            values
                .into_iter()
                .map(|(method, params, result)| Expected {
                    method,
                    params,
                    result,
                })
                .collect(),
        )))
    }

    pub fn one(method: &'static str, params: Value, result: Value) -> Self {
        Self::new([(method, params, result)])
    }

    pub fn assert_finished(&self) {
        assert!(
            self.0.lock().expect("script lock").is_empty(),
            "unconsumed RPC calls"
        );
    }
}

impl json_rpc::Client for Scripted {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<CallResult, Error>> {
        self.request_once(method, params)
    }

    fn request_once<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<CallResult, Error>> {
        Box::pin(async move {
            let expected = self
                .0
                .lock()
                .expect("script lock")
                .pop_front()
                .ok_or_else(|| Error {
                    kind: ErrorKind::InvalidRequest,
                    message: "unexpected call".into(),
                })?;
            assert_eq!(method, expected.method, "RPC method order");
            assert_eq!(params, expected.params, "RPC parameters");
            Ok(Ok(
                RawJson::from_serializable(&expected.result).expect("script result")
            ))
        })
    }

    fn batch<'a>(&'a self, _calls: Vec<Call>) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        Box::pin(async {
            Err(Error {
                kind: ErrorKind::InvalidRequest,
                message: "unexpected batch".into(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use super::*;
    use crate::{ErrorKind as SolanaErrorKind, RpcClient, RpcConfig};
    use json_rpc::Client as _;
    use serde_json::json;

    #[tokio::test]
    async fn owns_order_parameters_count_and_unexpected_call_failure() {
        let rpc = Scripted::new([
            ("first", json!([1]), json!(2)),
            ("second", json!({}), json!(3)),
        ]);
        rpc.request_once("first", json!([1]))
            .await
            .unwrap()
            .unwrap();
        rpc.request_once("second", json!({}))
            .await
            .unwrap()
            .unwrap();
        rpc.assert_finished();
        assert!(rpc.request_once("extra", json!([])).await.is_err());
    }

    #[tokio::test]
    async fn loopback_asserts_exact_wire_call_endpoint_and_response() {
        let endpoint = serve_once(|request| {
            assert_eq!(request["method"], "getHealth");
            assert_eq!(request["params"], json!([]));
            json!({"jsonrpc":"2.0","id":request["id"],"result":"ok"})
        });
        let config = RpcConfig::new(endpoint, Duration::from_secs(2), 1_024, 1_024)
            .expect("loopback config");
        RpcClient::connect(config)
            .expect("loopback client")
            .health()
            .await
            .expect("exact response");
    }

    #[tokio::test]
    async fn loopback_enforces_response_limit_after_one_call() {
        let endpoint = serve_once(
            |request| json!({"jsonrpc":"2.0","id":request["id"],"result":"x".repeat(512)}),
        );
        let config =
            RpcConfig::new(endpoint, Duration::from_secs(2), 1_024, 64).expect("bounded config");
        let error = RpcClient::connect(config)
            .expect("bounded client")
            .health()
            .await
            .unwrap_err();
        assert_eq!(error.kind(), SolanaErrorKind::ResponseTooLarge);
    }

    #[tokio::test]
    async fn loopback_request_is_cancelled_without_a_second_call() {
        let Held {
            endpoint,
            observed,
            release,
            thread,
        } = serve_held();
        let config =
            RpcConfig::new(endpoint, Duration::from_secs(2), 1_024, 1_024).expect("held config");
        let client = RpcClient::connect(config).expect("held client");
        let request = tokio::spawn(async move { client.health().await });
        let observed = tokio::task::spawn_blocking(move || {
            observed.recv_timeout(Duration::from_secs(2)).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(observed["method"], "getHealth");
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap())
            .await
            .unwrap();
    }

    fn serve_once(response: impl FnOnce(Value) -> Value + Send + 'static) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let address = listener.local_addr().expect("loopback address");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request");
            let request = read_request(&mut stream);
            write_response(&mut stream, &response(request));
        });
        format!("http://{address}")
    }

    struct Held {
        endpoint: String,
        observed: mpsc::Receiver<Value>,
        release: mpsc::Sender<()>,
        thread: thread::JoinHandle<()>,
    }

    fn serve_held() -> Held {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let address = listener.local_addr().expect("loopback address");
        let (observed_send, observed) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request");
            let request = read_request(&mut stream);
            observed_send.send(request.clone()).unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            write_response(
                &mut stream,
                &json!({"jsonrpc":"2.0","id":request["id"],"result":"ok"}),
            );
        });
        Held {
            endpoint: format!("http://{address}"),
            observed,
            release,
            thread,
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Value {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 2_048];
        loop {
            let count = stream.read(&mut chunk).expect("request bytes");
            assert_ne!(count, 0);
            bytes.extend_from_slice(&chunk[..count]);
            let Some(split) = bytes.windows(4).position(|value| value == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&bytes[..split]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .expect("content length");
            let start = split + 4;
            if bytes.len() >= start + length {
                return serde_json::from_slice(&bytes[start..start + length]).unwrap();
            }
        }
    }

    fn write_response(stream: &mut std::net::TcpStream, value: &Value) {
        let body = serde_json::to_vec(value).unwrap();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(&body).unwrap();
    }
}
