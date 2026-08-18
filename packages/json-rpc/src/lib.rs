//! Generic JSON-RPC 2.0 framing over a protocol-independent transport.
//!
//! Concrete chain methods and retry decisions for chain-level errors do not
//! belong here.

use std::{collections::HashMap, error::Error as StdError, fmt};

use http_support::client::{
    BoxFuture, Client as HttpClient, Error as HttpError, ErrorKind as HttpErrorKind,
    Request as HttpRequest, Response as HttpResponse,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, value::RawValue};

mod failover;

pub use failover::Failover;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(u64),
    String(String),
}

impl RequestId {
    fn to_value(&self) -> Value {
        match self {
            Self::Number(number) => Value::Number((*number).into()),
            Self::String(string) => Value::String(string.clone()),
        }
    }

    fn from_value(value: Value) -> Result<Self, Error> {
        match value {
            Value::Number(number) => number.as_u64().map(Self::Number).ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidResponse,
                    "JSON-RPC response ID must be an unsigned integer or string",
                )
            }),
            Value::String(string) => Ok(Self::String(string)),
            _ => Err(Error::new(
                ErrorKind::InvalidResponse,
                "JSON-RPC response ID must be an unsigned integer or string",
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawJson(pub Vec<u8>);

impl RawJson {
    pub fn new(bytes: Vec<u8>) -> Result<Self, Error> {
        serde_json::from_slice::<Value>(&bytes).map_err(|_| {
            Error::new(
                ErrorKind::InvalidRequest,
                "raw JSON value is not valid JSON",
            )
        })?;
        Ok(Self(bytes))
    }

    pub fn from_serializable<T>(value: &T) -> Result<Self, Error>
    where
        T: Serialize + ?Sized,
    {
        serde_json::to_vec(value).map(Self).map_err(|_| {
            Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC value could not be serialized",
            )
        })
    }

    pub fn deserialize<T>(&self) -> Result<T, Error>
    where
        T: DeserializeOwned,
    {
        serde_json::from_slice(&self.0).map_err(|_| {
            Error::new(
                ErrorKind::InvalidResponse,
                "JSON-RPC value does not match the requested type",
            )
        })
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn into_value(self, kind: ErrorKind) -> Result<Value, Error> {
        serde_json::from_slice(&self.0)
            .map_err(|_| Error::new(kind, "raw JSON value is not valid JSON"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    pub params: RawJson,
}

impl Request {
    pub fn new<T>(id: RequestId, method: impl Into<String>, params: &T) -> Result<Self, Error>
    where
        T: Serialize + ?Sized,
    {
        let method = method.into();
        if method.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC method must not be empty",
            ));
        }
        Ok(Self {
            id,
            method,
            params: RawJson::from_serializable(params)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub id: RequestId,
    pub result: Result<RawJson, Failure>,
}

impl Response {
    pub fn decode_result<T>(&self) -> Result<T, Error>
    where
        T: DeserializeOwned,
    {
        match &self.result {
            Ok(result) => result.deserialize(),
            Err(failure) => Err(Error::new(
                ErrorKind::RemoteFailure,
                format!("JSON-RPC request failed with code {}", failure.code),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: i64,
    pub message: String,
    pub data: Option<RawJson>,
}

impl Failure {
    /// JSON-RPC reserves this range for implementation-defined server errors.
    /// This is classification only; callers decide whether a concrete code is retryable.
    #[must_use]
    pub const fn is_server_error(&self) -> bool {
        self.code >= -32_099 && self.code <= -32_000
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidRequest,
    Transport(HttpErrorKind),
    HttpStatus(u16),
    InvalidResponse,
    ResponseMismatch,
    RemoteFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn from_transport(error: HttpError) -> Self {
        // Transport messages are expected to be sanitized. We still use a bounded,
        // local message so headers, bodies, and credential-bearing URLs never cross
        // the JSON-RPC error boundary.
        Self::new(
            ErrorKind::Transport(error.kind),
            match error.kind {
                HttpErrorKind::Timeout => "JSON-RPC transport timed out",
                HttpErrorKind::Unavailable => "JSON-RPC transport is unavailable",
                HttpErrorKind::Rejected => "JSON-RPC transport rejected the request",
                HttpErrorKind::InvalidResponse => "JSON-RPC transport returned an invalid response",
                HttpErrorKind::Other => "JSON-RPC transport failed",
            },
        )
    }

    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Transport(HttpErrorKind::Timeout)
                | ErrorKind::Transport(HttpErrorKind::Unavailable)
                | ErrorKind::HttpStatus(429 | 502 | 503 | 504)
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}

pub trait Client: Send + Sync {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, Error>>;

    fn batch<'a>(&'a self, requests: Vec<Request>) -> BoxFuture<'a, Result<Vec<Response>, Error>>;
}

#[derive(Clone)]
pub struct TransportClient<T> {
    transport: T,
    endpoint: String,
    headers: Vec<(String, String)>,
}

impl<T> fmt::Debug for TransportClient<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        formatter
            .debug_struct("TransportClient")
            .field("endpoint", &"[REDACTED]")
            .field("header_names", &header_names)
            .finish_non_exhaustive()
    }
}

impl<T> TransportClient<T> {
    #[must_use]
    pub fn new(transport: T, endpoint: impl Into<String>) -> Self {
        Self {
            transport,
            endpoint: endpoint.into(),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        }
    }

    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

impl<T> TransportClient<T>
where
    T: HttpClient,
{
    async fn send_body(&self, body: Vec<u8>) -> Result<HttpResponse, Error> {
        let response = self
            .transport
            .execute(HttpRequest {
                headers: self.headers.clone(),
                ..HttpRequest::post(self.endpoint.clone(), body)
            })
            .await
            .map_err(Error::from_transport)?;
        if !(200..300).contains(&response.status) {
            return Err(Error::new(
                ErrorKind::HttpStatus(response.status),
                "JSON-RPC endpoint returned a non-success HTTP status",
            ));
        }
        Ok(response)
    }
}

impl<T> Client for TransportClient<T>
where
    T: HttpClient,
{
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let expected_id = request.id.clone();
            let body = request.encode()?;
            let response = Response::decode(&self.send_body(body).await?.body)?;
            if response.id != expected_id {
                return Err(Error::new(
                    ErrorKind::ResponseMismatch,
                    "JSON-RPC response ID does not match the request",
                ));
            }
            Ok(response)
        })
    }

    fn batch<'a>(&'a self, requests: Vec<Request>) -> BoxFuture<'a, Result<Vec<Response>, Error>> {
        Box::pin(async move {
            if requests.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidRequest,
                    "JSON-RPC batch must contain at least one request",
                ));
            }

            let mut positions = HashMap::with_capacity(requests.len());
            for (position, request) in requests.iter().enumerate() {
                if positions.insert(request.id.clone(), position).is_some() {
                    return Err(Error::new(
                        ErrorKind::InvalidRequest,
                        "JSON-RPC batch request IDs must be unique",
                    ));
                }
            }

            let body = Request::encode_batch(requests)?;
            let responses = Response::decode_batch(&self.send_body(body).await?.body)?;
            let mut ordered: Vec<Option<Response>> = vec![None; positions.len()];
            for response in responses {
                let Some(position) = positions.get(&response.id).copied() else {
                    return Err(Error::new(
                        ErrorKind::ResponseMismatch,
                        "JSON-RPC batch contains an unknown response ID",
                    ));
                };
                if ordered[position].replace(response).is_some() {
                    return Err(Error::new(
                        ErrorKind::ResponseMismatch,
                        "JSON-RPC batch contains a duplicate response ID",
                    ));
                }
            }

            ordered
                .into_iter()
                .map(|response| {
                    response.ok_or_else(|| {
                        Error::new(
                            ErrorKind::ResponseMismatch,
                            "JSON-RPC batch is missing a response",
                        )
                    })
                })
                .collect()
        })
    }
}

impl Request {
    pub fn encode(self) -> Result<Vec<u8>, Error> {
        let value = self.into_value()?;
        serde_json::to_vec(&value).map_err(|_| {
            Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC request could not be encoded",
            )
        })
    }

    pub fn encode_batch(requests: Vec<Self>) -> Result<Vec<u8>, Error> {
        if requests.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC batch must contain at least one request",
            ));
        }
        let values = requests
            .into_iter()
            .map(Self::into_value)
            .collect::<Result<Vec<_>, _>>()?;
        serde_json::to_vec(&values).map_err(|_| {
            Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC batch could not be encoded",
            )
        })
    }

    fn into_value(self) -> Result<Value, Error> {
        if self.method.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC method must not be empty",
            ));
        }
        let params = self.params.into_value(ErrorKind::InvalidRequest)?;
        if !matches!(params, Value::Array(_) | Value::Object(_)) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "JSON-RPC params must be an array or object",
            ));
        }

        let mut object = Map::new();
        object.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
        object.insert("id".to_owned(), self.id.to_value());
        object.insert("method".to_owned(), Value::String(self.method));
        object.insert("params".to_owned(), params);
        Ok(Value::Object(object))
    }
}

impl Response {
    pub fn decode(body: &[u8]) -> Result<Self, Error> {
        let object: HashMap<String, Box<RawValue>> =
            serde_json::from_slice(body).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidResponse,
                    "JSON-RPC response body must be a valid JSON object",
                )
            })?;
        Self::from_object(object)
    }

    pub fn decode_batch(body: &[u8]) -> Result<Vec<Self>, Error> {
        let objects: Vec<HashMap<String, Box<RawValue>>> =
            serde_json::from_slice(body).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidResponse,
                    "JSON-RPC batch response must be an array of JSON objects",
                )
            })?;
        if objects.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidResponse,
                "JSON-RPC batch response must not be empty",
            ));
        }
        objects.into_iter().map(Self::from_object).collect()
    }

    fn from_object(mut object: HashMap<String, Box<RawValue>>) -> Result<Self, Error> {
        let version = object.remove("jsonrpc").ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidResponse,
                "JSON-RPC response is missing its version",
            )
        })?;
        if decode_raw::<String>(&version, "JSON-RPC response version must be a string")? != "2.0" {
            return Err(Error::new(
                ErrorKind::InvalidResponse,
                "JSON-RPC response must declare version 2.0",
            ));
        }
        let raw_id = object.remove("id").ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidResponse,
                "JSON-RPC response is missing an ID",
            )
        })?;
        let id = RequestId::from_value(decode_raw(
            &raw_id,
            "JSON-RPC response ID is not valid JSON",
        )?)?;

        let result = object.remove("result");
        let failure = object.remove("error");
        match (result, failure) {
            (Some(result), None) => Ok(Self {
                id,
                // RawValue points at the provider's exact result token. Copying its
                // bytes preserves object member order, internal whitespace, and
                // number spelling instead of round-tripping through Value.
                result: Ok(RawJson(result.get().as_bytes().to_vec())),
            }),
            (None, Some(failure)) => Ok(Self {
                id,
                result: Err(Failure::parse(&failure)?),
            }),
            _ => Err(Error::new(
                ErrorKind::InvalidResponse,
                "JSON-RPC response must contain exactly one of result or error",
            )),
        }
    }
}

impl Failure {
    fn parse(value: &RawValue) -> Result<Self, Error> {
        let mut object: HashMap<String, Box<RawValue>> = serde_json::from_str(value.get())
            .map_err(|_| {
                Error::new(
                    ErrorKind::InvalidResponse,
                    "JSON-RPC error must be an object",
                )
            })?;
        let code = object
            .remove("code")
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidResponse,
                    "JSON-RPC error is missing an integer code",
                )
            })
            .and_then(|value| decode_raw(&value, "JSON-RPC error is missing an integer code"))?;
        let message = object
            .remove("message")
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidResponse,
                    "JSON-RPC error is missing a string message",
                )
            })
            .and_then(|value| decode_raw(&value, "JSON-RPC error is missing a string message"))?;
        let data = object
            .remove("data")
            .map(|value| RawJson(value.get().as_bytes().to_vec()));
        Ok(Self {
            code,
            message,
            data,
        })
    }
}

fn decode_raw<T>(value: &RawValue, message: &'static str) -> Result<T, Error>
where
    T: DeserializeOwned,
{
    serde_json::from_str(value.get()).map_err(|_| Error::new(ErrorKind::InvalidResponse, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_typed_request_envelope() {
        let request = Request::new(RequestId::Number(7), "chain_method", &["first", "second"])
            .expect("typed request must encode");
        let encoded = request.encode().expect("request envelope must encode");
        let value: Value = serde_json::from_slice(&encoded).expect("encoded request must be JSON");

        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "chain_method");
        assert_eq!(value["params"], serde_json::json!(["first", "second"]));
    }

    #[test]
    fn parses_success_and_error_responses_without_losing_data() {
        let success = Response::decode(br#"{"jsonrpc":"2.0","id":1,"result":{"value":42}}"#)
            .expect("success response must parse");
        let value: Value = success
            .result
            .expect("response must contain a result")
            .deserialize()
            .expect("result must decode");
        assert_eq!(value, serde_json::json!({"value": 42}));

        let failed = Response::decode(
            br#"{"jsonrpc":"2.0","id":"a","error":{"code":-32005,"message":"busy","data":{"retryAfter":1}}}"#,
        )
        .expect("error response must parse");
        let failure = failed.result.expect_err("response must contain an error");
        assert_eq!(failure.code, -32005);
        assert!(failure.is_server_error());
        let data: Value = failure
            .data
            .expect("error data must be preserved")
            .deserialize()
            .expect("error data must decode");
        assert_eq!(data, serde_json::json!({"retryAfter": 1}));
    }

    #[test]
    fn preserves_exact_success_result_bytes() {
        let body = br#"{
            "id": 7,
            "result" : { "z" : 4.20e+01, "a" : [ 1,  2 ] },
            "jsonrpc": "2.0"
        }"#;
        let expected = br#"{ "z" : 4.20e+01, "a" : [ 1,  2 ] }"#;

        let response = Response::decode(body).expect("raw success response must parse");
        let result = response
            .result
            .expect("success response must have a result");

        assert_eq!(result.as_bytes(), expected);
    }

    #[test]
    fn preserves_each_exact_batch_result() {
        let body = br#"[
            {"jsonrpc":"2.0","id":2,"result": { "second" : 2.00e0, "first" : 1 }},
            {"result" : [ 3,  4 ], "id":1,"jsonrpc":"2.0"}
        ]"#;

        let responses = Response::decode_batch(body).expect("raw batch response must parse");

        assert_eq!(responses[0].id, RequestId::Number(2));
        assert_eq!(
            responses[0]
                .result
                .as_ref()
                .expect("first batch response must succeed")
                .as_bytes(),
            br#"{ "second" : 2.00e0, "first" : 1 }"#
        );
        assert_eq!(responses[1].id, RequestId::Number(1));
        assert_eq!(
            responses[1]
                .result
                .as_ref()
                .expect("second batch response must succeed")
                .as_bytes(),
            br#"[ 3,  4 ]"#
        );
    }

    #[test]
    fn rejects_malformed_protocol_envelopes() {
        let wrong_version = Response::decode(br#"{"jsonrpc":"1.0","id":1,"result":true}"#)
            .expect_err("wrong protocol version must fail");
        assert_eq!(wrong_version.kind, ErrorKind::InvalidResponse);

        let both = Response::decode(
            br#"{"jsonrpc":"2.0","id":1,"result":true,"error":{"code":-1,"message":"bad"}}"#,
        )
        .expect_err("result and error together must fail");
        assert_eq!(both.kind, ErrorKind::InvalidResponse);

        let invalid_params = Request {
            id: RequestId::Number(1),
            method: "method".to_owned(),
            params: RawJson(b"1".to_vec()),
        };
        assert_eq!(
            invalid_params
                .encode()
                .expect_err("scalar params must fail")
                .kind,
            ErrorKind::InvalidRequest
        );
    }

    #[test]
    fn retry_classification_is_limited_to_transient_transport_failures() {
        for kind in [HttpErrorKind::Timeout, HttpErrorKind::Unavailable] {
            let error = Error::from_transport(HttpError {
                kind,
                message: "must not be propagated: https://user:secret@example.invalid".to_owned(),
            });
            assert!(error.is_retryable());
            assert!(!error.message.contains("secret"));
        }

        let rejected = Error::from_transport(HttpError {
            kind: HttpErrorKind::Rejected,
            message: "rejected".to_owned(),
        });
        assert!(!rejected.is_retryable());
        assert!(Error::new(ErrorKind::HttpStatus(503), "busy").is_retryable());
        assert!(!Error::new(ErrorKind::HttpStatus(400), "bad").is_retryable());
    }

    #[test]
    fn debug_output_redacts_endpoint_and_header_values() {
        enum NeverTransport {
            Value,
        }
        let client =
            TransportClient::new(NeverTransport::Value, "https://user:secret@example.invalid")
                .with_header("authorization", "Bearer hidden");
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("Bearer hidden"));
        assert!(debug.contains("authorization"));
    }
}
