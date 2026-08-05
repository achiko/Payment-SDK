use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use signer::{
    Curve, Digest, KeyLocator, KeyProvisionRequest, KeyProvisioner, OperationId, PublicKeyFormat,
    SignRequest, SignablePayload, SignatureEncoding, SignatureScheme, Signer, SignerErrorKind,
    SignerStatus, UserInteraction,
};
use signer_remote::{
    BearerSecret, CAPABILITIES_PATH, PROVISION_PATH, PUBLIC_KEY_PATH, READINESS_PATH,
    RemoteRetryPolicy, RemoteSignerClient, RemoteSignerConfig, RemoteSignerConfigErrorKind,
    RemoteSignerEndpoint, SIGN_PATH, wire,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const SECRET: &str = "test-custody-secret";

#[derive(Clone, Default)]
struct TestState {
    authorization: Arc<Mutex<Vec<String>>>,
    operations: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    provision_attempts: Arc<AtomicUsize>,
    sign_attempts: Arc<AtomicUsize>,
    public_key_attempts: Arc<AtomicUsize>,
}

struct TestServer {
    endpoint: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_server(state: TestState) -> TestServer {
    let router = Router::new()
        .route(&format!("/{CAPABILITIES_PATH}"), get(capabilities))
        .route(&format!("/{READINESS_PATH}"), get(readiness))
        .route(&format!("/{PROVISION_PATH}"), post(provision))
        .route(&format!("/{PUBLIC_KEY_PATH}"), post(public_key))
        .route(&format!("/{SIGN_PATH}"), post(sign))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener must bind");
    let address = listener.local_addr().expect("test listener has an address");
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("test server must run");
    });
    TestServer {
        endpoint: format!("http://{address}"),
        task,
    }
}

async fn connect(
    endpoint: &str,
    request_timeout: Duration,
    max_response_bytes: usize,
    retry_attempts: u32,
) -> RemoteSignerClient {
    let endpoint = RemoteSignerEndpoint::new(endpoint).expect("test endpoint must be valid");
    let secret = BearerSecret::new(SECRET).expect("test bearer secret must be valid");
    let retry = RemoteRetryPolicy::new(
        retry_attempts,
        Duration::from_millis(1),
        Duration::from_millis(2),
    )
    .expect("test retry policy must be valid");
    let config = RemoteSignerConfig::new(endpoint, secret)
        .with_timeouts(request_timeout, request_timeout)
        .expect("test timeouts must be valid")
        .with_max_response_bytes(max_response_bytes)
        .expect("test response limit must be valid")
        .with_retry_policy(retry);
    RemoteSignerClient::connect(config)
        .await
        .expect("test client must connect")
}

async fn capabilities(
    State(state): State<TestState>,
    headers: HeaderMap,
) -> Json<wire::CapabilitiesResponse> {
    record_auth(&state, &headers);
    Json(wire::CapabilitiesResponse {
        curves: vec![wire::Curve::Secp256k1],
        schemes: vec![
            wire::SignatureScheme::EcdsaSecp256k1,
            wire::SignatureScheme::SchnorrSecp256k1,
        ],
        can_sign_messages: true,
        can_sign_digests: true,
        requires_user_interaction: false,
    })
}

async fn readiness(
    State(state): State<TestState>,
    headers: HeaderMap,
) -> Json<wire::ReadinessResponse> {
    record_auth(&state, &headers);
    Json(wire::ReadinessResponse {
        status: wire::ReadinessStatus::Available,
    })
}

async fn provision(
    State(state): State<TestState>,
    headers: HeaderMap,
    Json(request): Json<wire::ProvisionRequest>,
) -> Response {
    record_auth(&state, &headers);
    state.provision_attempts.fetch_add(1, Ordering::Relaxed);
    if let Some(conflict) = operation_conflict(&state, &request.operation_id, &request) {
        return conflict;
    }
    Json(wire::ProvisionResponse {
        locator: wire::KeyLocator::Identifier {
            value: format!("remote:{}", request.purpose),
        },
        public_key: wire::PublicKey {
            curve: request.curve,
            format: request.public_key_format,
            bytes_hex: public_key_hex(request.public_key_format),
        },
    })
    .into_response()
}

async fn public_key(
    State(state): State<TestState>,
    headers: HeaderMap,
    Json(request): Json<wire::PublicKeyRequest>,
) -> Response {
    record_auth(&state, &headers);
    state.public_key_attempts.fetch_add(1, Ordering::Relaxed);
    let wire::KeyLocator::Identifier { value } = &request.locator else {
        return error_response(StatusCode::NOT_FOUND, "key_not_found", false);
    };
    if value == "missing" {
        return error_response(StatusCode::NOT_FOUND, "key_not_found", false);
    }
    if value == "transient" {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "unavailable", true);
    }
    let bytes_hex = if value == "oversized" {
        format!("0x{}", "aa".repeat(4_096))
    } else {
        public_key_hex(request.format)
    };
    Json(wire::PublicKeyResponse {
        public_key: wire::PublicKey {
            curve: request.curve,
            format: request.format,
            bytes_hex,
        },
    })
    .into_response()
}

async fn sign(
    State(state): State<TestState>,
    headers: HeaderMap,
    Json(request): Json<wire::SignRequest>,
) -> Response {
    record_auth(&state, &headers);
    let attempt = state.sign_attempts.fetch_add(1, Ordering::Relaxed) + 1;
    if request.operation_id == "timeout-sign" {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    if request.operation_id == "retry-sign" && attempt == 1 {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "unavailable", true);
    }
    if let Some(conflict) = operation_conflict(&state, &request.operation_id, &request) {
        return conflict;
    }
    Json(wire::SignResponse {
        scheme: request.scheme,
        encoding: request.encoding,
        bytes_hex: signature_hex(request.encoding),
    })
    .into_response()
}

fn record_auth(state: &TestState, headers: &HeaderMap) {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("[missing]")
        .to_owned();
    state
        .authorization
        .lock()
        .expect("test authorization lock must be available")
        .push(value);
}

fn operation_conflict<T: serde::Serialize>(
    state: &TestState,
    operation_id: &str,
    request: &T,
) -> Option<Response> {
    let body = serde_json::to_vec(request).expect("test request must serialize");
    let mut operations = state
        .operations
        .lock()
        .expect("test operation lock must be available");
    match operations.get(operation_id) {
        Some(existing) if existing != &body => Some(error_response(
            StatusCode::CONFLICT,
            "operation_changed",
            false,
        )),
        Some(_) => None,
        None => {
            operations.insert(operation_id.to_owned(), body);
            None
        }
    }
}

fn error_response(status: StatusCode, code: &str, retryable: bool) -> Response {
    (
        status,
        Json(wire::ErrorResponse {
            code: code.to_owned(),
            message: format!("untrusted backend detail containing {SECRET}"),
            retryable,
        }),
    )
        .into_response()
}

fn public_key_hex(format: wire::PublicKeyFormat) -> String {
    let bytes = match format {
        wire::PublicKeyFormat::Compressed => {
            let mut bytes = vec![2];
            bytes.extend([7; 32]);
            bytes
        }
        wire::PublicKeyFormat::Uncompressed => {
            let mut bytes = vec![4];
            bytes.extend([7; 64]);
            bytes
        }
        wire::PublicKeyFormat::XOnly => vec![7; 32],
        wire::PublicKeyFormat::Raw => vec![7; 64],
    };
    format!("0x{}", hex::encode(bytes))
}

fn signature_hex(encoding: wire::SignatureEncoding) -> String {
    let length = match encoding {
        wire::SignatureEncoding::Recoverable => 65,
        wire::SignatureEncoding::Der => 70,
        wire::SignatureEncoding::Compact | wire::SignatureEncoding::Raw => 64,
    };
    format!("0x{}", hex::encode(vec![9; length]))
}

fn operation(value: &str) -> OperationId {
    OperationId::new(value).expect("test operation ID must be valid")
}

fn provision_request(operation_id: &str, purpose: &str) -> KeyProvisionRequest {
    KeyProvisionRequest {
        operation_id: operation(operation_id),
        curve: Curve::Secp256k1,
        public_key_format: PublicKeyFormat::Compressed,
        purpose: purpose.to_owned(),
    }
}

fn sign_request(operation_id: &str) -> SignRequest {
    SignRequest {
        operation_id: operation(operation_id),
        key: KeyLocator::Identifier("remote:deposit".to_owned()),
        payload: SignablePayload::Digest(Digest { bytes: vec![3; 32] }),
        scheme: SignatureScheme::EcdsaSecp256k1,
        encoding: SignatureEncoding::Recoverable,
        key_tweak: None,
        user_interaction: UserInteraction::NotRequired,
    }
}

#[tokio::test]
async fn authenticated_client_implements_provision_sign_lookup_and_readiness() {
    let state = TestState::default();
    let server = spawn_server(state.clone()).await;
    let client = connect(&server.endpoint, Duration::from_secs(1), 16 * 1024, 2).await;

    let capabilities = client.capabilities();
    assert_eq!(capabilities.curves, vec![Curve::Secp256k1]);
    assert!(capabilities.can_sign_messages);
    assert!(capabilities.can_sign_digests);
    assert_eq!(
        client.status().await.expect("readiness should succeed"),
        SignerStatus::Available
    );

    let provisioned = client
        .provision(provision_request("provision-deposit", "deposit"))
        .await
        .expect("provision should succeed");
    assert_eq!(
        provisioned.locator,
        KeyLocator::Identifier("remote:deposit".to_owned())
    );
    assert_eq!(provisioned.public_key.bytes.len(), 33);

    let public_key = client
        .public_key(
            &provisioned.locator,
            Curve::Secp256k1,
            PublicKeyFormat::Compressed,
        )
        .await
        .expect("public key lookup should succeed");
    assert_eq!(public_key, provisioned.public_key);

    let signature = client
        .sign(sign_request("sign-deposit"))
        .await
        .expect("sign should succeed");
    assert_eq!(signature.bytes.len(), 65);

    let authorization = state
        .authorization
        .lock()
        .expect("test authorization lock must be available");
    assert!(!authorization.is_empty());
    assert!(
        authorization
            .iter()
            .all(|value| value == &format!("Bearer {SECRET}"))
    );
}

#[tokio::test]
async fn operation_replay_is_stable_and_changed_content_maps_to_conflict() {
    let state = TestState::default();
    let server = spawn_server(state.clone()).await;
    let client = connect(&server.endpoint, Duration::from_secs(1), 16 * 1024, 2).await;

    let request = provision_request("provision-replay", "deposit-a");
    let first = client
        .provision(request.clone())
        .await
        .expect("first provision should succeed");
    let replay = client
        .provision(request)
        .await
        .expect("identical provision replay should succeed");
    assert_eq!(first, replay);

    let error = client
        .provision(provision_request("provision-replay", "deposit-b"))
        .await
        .expect_err("changed operation content must conflict");
    assert_eq!(error.kind, SignerErrorKind::InvalidRequest);
    assert!(error.message.contains("operation ID"));
    assert!(!error.message.contains(SECRET));
}

#[tokio::test]
async fn retries_only_operation_id_calls() {
    let state = TestState::default();
    let server = spawn_server(state.clone()).await;
    let client = connect(&server.endpoint, Duration::from_secs(1), 16 * 1024, 2).await;

    let signature = client
        .sign(sign_request("retry-sign"))
        .await
        .expect("idempotent sign should retry and succeed");
    assert_eq!(signature.bytes.len(), 65);
    assert_eq!(state.sign_attempts.load(Ordering::Relaxed), 2);

    let error = client
        .public_key(
            &KeyLocator::Identifier("transient".to_owned()),
            Curve::Secp256k1,
            PublicKeyFormat::Compressed,
        )
        .await
        .expect_err("public key lookup must not retry");
    assert_eq!(error.kind, SignerErrorKind::Unavailable);
    assert_eq!(state.public_key_attempts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn timeout_and_remote_errors_have_structured_sanitized_mapping() {
    let state = TestState::default();
    let server = spawn_server(state).await;
    let client = connect(&server.endpoint, Duration::from_millis(20), 16 * 1024, 1).await;

    let timeout = client
        .sign(sign_request("timeout-sign"))
        .await
        .expect_err("slow sign must time out");
    assert_eq!(timeout.kind, SignerErrorKind::Unavailable);
    assert!(timeout.message.contains("timed out"));
    assert!(!timeout.message.contains(&server.endpoint));
    assert!(!timeout.message.contains(SECRET));

    let missing = client
        .public_key(
            &KeyLocator::Identifier("missing".to_owned()),
            Curve::Secp256k1,
            PublicKeyFormat::Compressed,
        )
        .await
        .expect_err("missing key must fail");
    assert_eq!(missing.kind, SignerErrorKind::KeyNotFound);
    assert!(!missing.message.contains(SECRET));
}

#[tokio::test]
async fn debug_output_redacts_endpoint_bearer_and_operation_ids() {
    let state = TestState::default();
    let server = spawn_server(state).await;
    let endpoint = RemoteSignerEndpoint::new(&server.endpoint).expect("endpoint must be valid");
    let secret = BearerSecret::new(SECRET).expect("secret must be valid");
    let config = RemoteSignerConfig::new(endpoint, secret);
    let config_debug = format!("{config:?}");
    assert!(!config_debug.contains(&server.endpoint));
    assert!(!config_debug.contains(SECRET));

    let client = RemoteSignerClient::connect(config)
        .await
        .expect("client must connect");
    let client_debug = format!("{client:?}");
    assert!(!client_debug.contains(&server.endpoint));
    assert!(!client_debug.contains(SECRET));
    assert!(
        !format!("{:?}", operation("sensitive-business-operation"))
            .contains("sensitive-business-operation")
    );
}

#[tokio::test]
async fn response_size_limit_is_enforced_before_json_is_exposed() {
    let state = TestState::default();
    let server = spawn_server(state).await;
    let client = connect(&server.endpoint, Duration::from_secs(1), 512, 1).await;

    let error = client
        .public_key(
            &KeyLocator::Identifier("oversized".to_owned()),
            Curve::Secp256k1,
            PublicKeyFormat::Compressed,
        )
        .await
        .expect_err("oversized response must fail");
    assert_eq!(error.kind, SignerErrorKind::Other);
    assert!(error.message.contains("size limit"));
    assert!(!error.message.contains("aa"));
}

#[test]
fn endpoint_secret_and_limits_are_validated() {
    let insecure = RemoteSignerEndpoint::new("http://example.com")
        .expect_err("non-loopback HTTP must be rejected");
    assert_eq!(insecure.kind, RemoteSignerConfigErrorKind::InsecureEndpoint);

    let invalid_secret = BearerSecret::new("contains whitespace")
        .expect_err("whitespace in bearer credential must be rejected");
    assert_eq!(
        invalid_secret.kind,
        RemoteSignerConfigErrorKind::InvalidBearerSecret
    );

    let endpoint =
        RemoteSignerEndpoint::new("http://127.0.0.1:8080").expect("loopback HTTP is valid");
    let secret = BearerSecret::new(SECRET).expect("test secret is valid");
    let invalid_timeout = RemoteSignerConfig::new(endpoint, secret)
        .with_timeouts(Duration::ZERO, Duration::from_secs(1))
        .expect_err("zero timeout must be rejected");
    assert_eq!(
        invalid_timeout.kind,
        RemoteSignerConfigErrorKind::InvalidTimeout
    );
}
