//! Loopback-only ephemeral custody adapter for local development.
//!
//! Private keys live only in this process and are destroyed when it exits.
//! This binary is not a durable or production custody implementation.

use std::{collections::BTreeMap, error::Error, fmt, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::{Args, Parser, Subcommand};
use http_support::{
    AuthenticationMode, BearerToken, HealthState, HttpServerConfig, RequestLimits,
    TransportSecurity,
};
use signer::{
    ChildIndex, Curve, DerivationPath, Digest, KeyLocator, KeyProvisionRequest, KeyProvisioner,
    KeyTweak, KeyTweakKind, OperationId, PublicKey, PublicKeyFormat, SignRequest, SignablePayload,
    SignatureEncoding, SignatureScheme, Signer, SignerError, SignerErrorKind, SignerStatus,
    UserInteraction,
};
use signer_local::LocalSigner;
use signer_remote::{
    CAPABILITIES_PATH, PROVISION_PATH, PUBLIC_KEY_PATH, READINESS_PATH, SIGN_PATH, wire,
};
use telemetry::{Attribute, PrometheusTelemetry, Telemetry};
use tokio::sync::{Mutex, watch};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

type AppError = Box<dyn Error + Send + Sync>;
type AppResult<T> = Result<T, AppError>;

#[derive(Parser)]
#[command(
    name = "custody-worker",
    version,
    about = "Ephemeral loopback custody for local development only"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the mode-aware v1 custody API on loopback.
    Serve(ServeOptions),
}

#[derive(Args, Clone)]
struct ServeOptions {
    #[arg(long, env = "CUSTODY_BIND", default_value = "127.0.0.1:8181")]
    bind: SocketAddr,

    #[arg(long, env = "CUSTODY_METRICS_BIND", default_value = "127.0.0.1:9093")]
    metrics_bind: SocketAddr,

    /// `true` requires a bearer; `false` globally trusts every reachable caller.
    #[arg(
        long = "strict-authentication-mode",
        env = "STRICT_AUTHENTICATION_MODE"
    )]
    authentication_mode: AuthenticationMode,

    #[arg(long, env = "CUSTODY_BEARER_TOKEN", hide_env_values = true)]
    bearer_token: Option<String>,

    #[arg(long, env = "CUSTODY_MAX_REQUEST_BODY_BYTES", default_value_t = 65_536)]
    max_request_body_bytes: usize,

    #[arg(long, env = "CUSTODY_SHUTDOWN_GRACE_SECONDS", default_value_t = 10)]
    shutdown_grace_seconds: u64,
}

impl ServeOptions {
    fn server_config(&self) -> AppResult<HttpServerConfig> {
        if !self.bind.ip().is_loopback() || !self.metrics_bind.ip().is_loopback() {
            return Err(Box::new(ConfigError(
                "local custody API and metrics may bind only to loopback addresses".to_owned(),
            )));
        }
        if self.max_request_body_bytes == 0 || self.shutdown_grace_seconds == 0 {
            return Err(Box::new(ConfigError(
                "request-body and shutdown-grace limits must be greater than zero".to_owned(),
            )));
        }
        let token = match self.authentication_mode {
            AuthenticationMode::Strict => Some(BearerToken::new(
                self.bearer_token.as_deref().ok_or_else(|| {
                    ConfigError(
                        "CUSTODY_BEARER_TOKEN is required in strict authentication mode".to_owned(),
                    )
                })?,
            )?),
            AuthenticationMode::GlobalTrusted => None,
        };
        let limits = RequestLimits::new(self.max_request_body_bytes, 1, 1)?;
        let config = HttpServerConfig::new(
            self.bind,
            TransportSecurity::PlaintextLoopback,
            token,
            limits,
        )
        .with_authentication_mode(self.authentication_mode);
        config.validate()?;
        Ok(config)
    }

    fn metrics_server_config(&self) -> AppResult<HttpServerConfig> {
        let config = HttpServerConfig::new(
            self.metrics_bind,
            TransportSecurity::PlaintextLoopback,
            None,
            RequestLimits::new(self.max_request_body_bytes, 1, 1)?,
        )
        .with_authentication_mode(AuthenticationMode::GlobalTrusted);
        config.validate()?;
        Ok(config)
    }

    const fn shutdown_grace(&self) -> Duration {
        Duration::from_secs(self.shutdown_grace_seconds)
    }
}

#[derive(Clone)]
struct AppState {
    signer: Arc<LocalSigner>,
    authentication_mode: AuthenticationMode,
    operations: Arc<Mutex<BTreeMap<String, StoredOperation>>>,
}

#[derive(Clone)]
enum StoredOperation {
    Provision {
        request: wire::ProvisionRequest,
        response: wire::ProvisionResponse,
    },
    Sign {
        request: wire::SignRequest,
        response: wire::SignResponse,
    },
}

#[tokio::main]
async fn main() -> AppResult<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .try_init()?;

    match Cli::parse().command {
        Command::Serve(options) => serve(options).await,
    }
}

async fn serve(options: ServeOptions) -> AppResult<()> {
    let server_config = options.server_config()?;
    let metrics_server_config = options.metrics_server_config()?;
    let telemetry = PrometheusTelemetry::install()?;
    telemetry.gauge(
        "payment_sdk_strict_authentication_mode",
        if options.authentication_mode.is_strict() {
            1.0
        } else {
            0.0
        },
        &[Attribute {
            key: "service".to_owned(),
            value: "custody".to_owned(),
        }],
    );
    let state = AppState {
        signer: Arc::new(LocalSigner::ephemeral_for_testing()),
        authentication_mode: options.authentication_mode,
        operations: Arc::new(Mutex::new(BTreeMap::new())),
    };
    let protected = Router::new()
        .route(&format!("/{CAPABILITIES_PATH}"), get(capabilities))
        .route(&format!("/{READINESS_PATH}"), get(readiness))
        .route(&format!("/{PROVISION_PATH}"), post(provision))
        .route(&format!("/{PUBLIC_KEY_PATH}"), post(public_key))
        .route(&format!("/{SIGN_PATH}"), post(sign))
        .with_state(state);
    let health = HealthState::new(true);
    let router = http_support::service_router(protected, &server_config, health.clone())?;
    let metrics_router = Router::new()
        .route("/metrics", get(metrics))
        .with_state(telemetry);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let api_server =
        http_support::serve(router, &server_config, shutdown_signal(shutdown_rx.clone()));
    let metrics_server = http_support::serve(
        metrics_router,
        &metrics_server_config,
        shutdown_signal(shutdown_rx),
    );
    tokio::pin!(api_server);
    tokio::pin!(metrics_server);

    if options.authentication_mode == AuthenticationMode::GlobalTrusted {
        warn!(
            ignored_bearer_variable = "CUSTODY_BEARER_TOKEN",
            "STRICT AUTHENTICATION IS DISABLED: every reachable custody caller is globally trusted"
        );
    }
    warn!("ephemeral local custody is active; all keys will be lost when this process exits");
    info!(
        bind = %options.bind,
        metrics_bind = %options.metrics_bind,
        authentication_mode = %options.authentication_mode,
        "local custody is ready"
    );

    tokio::select! {
        result = &mut api_server => return result.map_err(|error| Box::new(error) as AppError),
        result = &mut metrics_server => return result.map_err(|error| Box::new(error) as AppError),
        result = termination_signal() => result?,
    }

    health.set_ready(false);
    let _ = shutdown_tx.send(true);
    let shutdown = async {
        let (api_result, metrics_result) = tokio::join!(&mut api_server, &mut metrics_server);
        api_result.map_err(|error| Box::new(error) as AppError)?;
        metrics_result.map_err(|error| Box::new(error) as AppError)
    };
    match tokio::time::timeout(options.shutdown_grace(), shutdown).await {
        Ok(result) => result,
        Err(_) => Err(Box::new(ConfigError(
            "local custody graceful shutdown deadline expired".to_owned(),
        ))),
    }
}

async fn capabilities(State(state): State<AppState>) -> Json<wire::CapabilitiesResponse> {
    let capabilities = state.signer.capabilities();
    Json(wire::CapabilitiesResponse {
        authentication_mode: state.authentication_mode.as_str().to_owned(),
        curves: capabilities.curves.into_iter().map(curve_to_wire).collect(),
        schemes: capabilities
            .schemes
            .into_iter()
            .map(signature_scheme_to_wire)
            .collect(),
        key_tweaks: capabilities
            .key_tweaks
            .into_iter()
            .map(|value| match value {
                KeyTweakKind::Secp256k1Add => wire::KeyTweakKind::Secp256k1Add,
            })
            .collect(),
        can_sign_messages: capabilities.can_sign_messages,
        can_sign_digests: capabilities.can_sign_digests,
        requires_user_interaction: capabilities.requires_user_interaction,
    })
}

async fn readiness(State(state): State<AppState>) -> Response {
    match state.signer.status().await {
        Ok(status) => Json(wire::ReadinessResponse {
            authentication_mode: state.authentication_mode.as_str().to_owned(),
            status: match status {
                SignerStatus::Available => wire::ReadinessStatus::Available,
                SignerStatus::InteractionRequired => wire::ReadinessStatus::InteractionRequired,
                SignerStatus::Unavailable { .. } => wire::ReadinessStatus::Unavailable,
            },
        })
        .into_response(),
        Err(error) => signer_error_response(error),
    }
}

async fn metrics(State(telemetry): State<PrometheusTelemetry>) -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        telemetry.render(),
    )
}

async fn provision(
    State(state): State<AppState>,
    Json(request): Json<wire::ProvisionRequest>,
) -> Response {
    let mut operations = state.operations.lock().await;
    if let Some(stored) = operations.get(&request.operation_id) {
        return match stored {
            StoredOperation::Provision {
                request: original,
                response,
            } if original == &request => Json(response.clone()).into_response(),
            _ => operation_changed(),
        };
    }

    let operation_id = match OperationId::new(request.operation_id.clone()) {
        Ok(value) => value,
        Err(error) => return signer_error_response(error),
    };
    let provisioned = match state
        .signer
        .provision(KeyProvisionRequest {
            operation_id,
            curve: curve_from_wire(request.curve),
            public_key_format: public_key_format_from_wire(request.public_key_format),
            purpose: request.purpose.clone(),
        })
        .await
    {
        Ok(value) => value,
        Err(error) => return signer_error_response(error),
    };
    let response = wire::ProvisionResponse {
        locator: locator_to_wire(provisioned.locator),
        public_key: public_key_to_wire(provisioned.public_key),
    };
    operations.insert(
        request.operation_id.clone(),
        StoredOperation::Provision {
            request,
            response: response.clone(),
        },
    );
    Json(response).into_response()
}

async fn public_key(
    State(state): State<AppState>,
    Json(request): Json<wire::PublicKeyRequest>,
) -> Response {
    let locator = match locator_from_wire(request.locator) {
        Ok(value) => value,
        Err(error) => return signer_error_response(error),
    };
    match state
        .signer
        .public_key(
            &locator,
            curve_from_wire(request.curve),
            public_key_format_from_wire(request.format),
        )
        .await
    {
        Ok(value) => Json(wire::PublicKeyResponse {
            public_key: public_key_to_wire(value),
        })
        .into_response(),
        Err(error) => signer_error_response(error),
    }
}

async fn sign(State(state): State<AppState>, Json(request): Json<wire::SignRequest>) -> Response {
    let mut operations = state.operations.lock().await;
    if let Some(stored) = operations.get(&request.operation_id) {
        return match stored {
            StoredOperation::Sign {
                request: original,
                response,
            } if original == &request => Json(response.clone()).into_response(),
            _ => operation_changed(),
        };
    }

    let sign_request = match sign_request_from_wire(request.clone()) {
        Ok(value) => value,
        Err(error) => return signer_error_response(error),
    };
    let signature = match state.signer.sign(sign_request).await {
        Ok(value) => value,
        Err(error) => return signer_error_response(error),
    };
    let response = wire::SignResponse {
        scheme: signature_scheme_to_wire(signature.scheme),
        encoding: signature_encoding_to_wire(signature.encoding),
        bytes_hex: encode_hex(&signature.bytes),
    };
    operations.insert(
        request.operation_id.clone(),
        StoredOperation::Sign {
            request,
            response: response.clone(),
        },
    );
    Json(response).into_response()
}

fn sign_request_from_wire(request: wire::SignRequest) -> Result<SignRequest, SignerError> {
    Ok(SignRequest {
        operation_id: OperationId::new(request.operation_id)?,
        key: locator_from_wire(request.locator)?,
        payload: match request.payload {
            wire::SignablePayload::Message { bytes_hex } => {
                SignablePayload::Message(decode_hex(&bytes_hex, "message")?)
            }
            wire::SignablePayload::Digest { bytes_hex } => SignablePayload::Digest(Digest {
                bytes: decode_hex(&bytes_hex, "digest")?,
            }),
        },
        scheme: signature_scheme_from_wire(request.scheme),
        encoding: signature_encoding_from_wire(request.encoding),
        key_tweak: request
            .key_tweak
            .map(|tweak| match tweak {
                wire::KeyTweak::Secp256k1Add { scalar_hex } => {
                    let bytes = decode_hex(&scalar_hex, "key tweak")?;
                    let scalar = bytes.try_into().map_err(|_| {
                        invalid_request("secp256k1 key tweak must contain exactly 32 bytes")
                    })?;
                    Ok(KeyTweak::Secp256k1Add(scalar))
                }
            })
            .transpose()?,
        user_interaction: match request.user_interaction {
            wire::UserInteraction::NotRequired => UserInteraction::NotRequired,
            wire::UserInteraction::Allowed => UserInteraction::Allowed,
            wire::UserInteraction::Required => UserInteraction::Required,
        },
    })
}

fn locator_from_wire(locator: wire::KeyLocator) -> Result<KeyLocator, SignerError> {
    match locator {
        wire::KeyLocator::Identifier { value } if value.trim().is_empty() => {
            Err(invalid_request("key locator identifier must not be empty"))
        }
        wire::KeyLocator::Identifier { value } => Ok(KeyLocator::Identifier(value)),
        wire::KeyLocator::DerivationPath { children } => {
            Ok(KeyLocator::DerivationPath(DerivationPath(
                children
                    .into_iter()
                    .map(|child| ChildIndex {
                        index: child.index,
                        hardened: child.hardened,
                    })
                    .collect(),
            )))
        }
    }
}

fn locator_to_wire(locator: KeyLocator) -> wire::KeyLocator {
    match locator {
        KeyLocator::Identifier(value) => wire::KeyLocator::Identifier { value },
        KeyLocator::DerivationPath(path) => wire::KeyLocator::DerivationPath {
            children: path
                .0
                .into_iter()
                .map(|child| wire::ChildIndex {
                    index: child.index,
                    hardened: child.hardened,
                })
                .collect(),
        },
    }
}

fn public_key_to_wire(public_key: PublicKey) -> wire::PublicKey {
    wire::PublicKey {
        curve: curve_to_wire(public_key.curve),
        format: public_key_format_to_wire(public_key.format),
        bytes_hex: encode_hex(&public_key.bytes),
    }
}

fn curve_to_wire(value: Curve) -> wire::Curve {
    match value {
        Curve::Secp256k1 => wire::Curve::Secp256k1,
        Curve::Ed25519 => wire::Curve::Ed25519,
        Curve::NistP256 => wire::Curve::NistP256,
    }
}

fn curve_from_wire(value: wire::Curve) -> Curve {
    match value {
        wire::Curve::Secp256k1 => Curve::Secp256k1,
        wire::Curve::Ed25519 => Curve::Ed25519,
        wire::Curve::NistP256 => Curve::NistP256,
    }
}

fn public_key_format_to_wire(value: PublicKeyFormat) -> wire::PublicKeyFormat {
    match value {
        PublicKeyFormat::Compressed => wire::PublicKeyFormat::Compressed,
        PublicKeyFormat::Uncompressed => wire::PublicKeyFormat::Uncompressed,
        PublicKeyFormat::XOnly => wire::PublicKeyFormat::XOnly,
        PublicKeyFormat::Raw => wire::PublicKeyFormat::Raw,
    }
}

fn public_key_format_from_wire(value: wire::PublicKeyFormat) -> PublicKeyFormat {
    match value {
        wire::PublicKeyFormat::Compressed => PublicKeyFormat::Compressed,
        wire::PublicKeyFormat::Uncompressed => PublicKeyFormat::Uncompressed,
        wire::PublicKeyFormat::XOnly => PublicKeyFormat::XOnly,
        wire::PublicKeyFormat::Raw => PublicKeyFormat::Raw,
    }
}

fn signature_scheme_to_wire(value: SignatureScheme) -> wire::SignatureScheme {
    match value {
        SignatureScheme::EcdsaSecp256k1 => wire::SignatureScheme::EcdsaSecp256k1,
        SignatureScheme::SchnorrSecp256k1 => wire::SignatureScheme::SchnorrSecp256k1,
        SignatureScheme::Ed25519 => wire::SignatureScheme::Ed25519,
        SignatureScheme::EcdsaNistP256 => wire::SignatureScheme::EcdsaNistP256,
    }
}

fn signature_scheme_from_wire(value: wire::SignatureScheme) -> SignatureScheme {
    match value {
        wire::SignatureScheme::EcdsaSecp256k1 => SignatureScheme::EcdsaSecp256k1,
        wire::SignatureScheme::SchnorrSecp256k1 => SignatureScheme::SchnorrSecp256k1,
        wire::SignatureScheme::Ed25519 => SignatureScheme::Ed25519,
        wire::SignatureScheme::EcdsaNistP256 => SignatureScheme::EcdsaNistP256,
    }
}

fn signature_encoding_to_wire(value: SignatureEncoding) -> wire::SignatureEncoding {
    match value {
        SignatureEncoding::Der => wire::SignatureEncoding::Der,
        SignatureEncoding::Compact => wire::SignatureEncoding::Compact,
        SignatureEncoding::Recoverable => wire::SignatureEncoding::Recoverable,
        SignatureEncoding::Raw => wire::SignatureEncoding::Raw,
    }
}

fn signature_encoding_from_wire(value: wire::SignatureEncoding) -> SignatureEncoding {
    match value {
        wire::SignatureEncoding::Der => SignatureEncoding::Der,
        wire::SignatureEncoding::Compact => SignatureEncoding::Compact,
        wire::SignatureEncoding::Recoverable => SignatureEncoding::Recoverable,
        wire::SignatureEncoding::Raw => SignatureEncoding::Raw,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn decode_hex(value: &str, field: &str) -> Result<Vec<u8>, SignerError> {
    let encoded = value
        .strip_prefix("0x")
        .ok_or_else(|| invalid_request(format!("{field} must be 0x-prefixed hex")))?;
    if encoded.is_empty() {
        return Err(invalid_request(format!("{field} must not be empty")));
    }
    hex::decode(encoded).map_err(|_| invalid_request(format!("{field} contains invalid hex")))
}

fn invalid_request(message: impl Into<String>) -> SignerError {
    SignerError {
        kind: SignerErrorKind::InvalidRequest,
        message: message.into(),
    }
}

fn operation_changed() -> Response {
    error_response(
        StatusCode::CONFLICT,
        "operation_changed",
        "operation ID was reused with different request content",
        false,
    )
}

fn signer_error_response(error: SignerError) -> Response {
    let (status, code, message, retryable) = match error.kind {
        SignerErrorKind::KeyNotFound => (
            StatusCode::NOT_FOUND,
            "key_not_found",
            "key was not found",
            false,
        ),
        SignerErrorKind::UnsupportedCurve => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_curve",
            "curve is not supported",
            false,
        ),
        SignerErrorKind::UnsupportedScheme => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_scheme",
            "signature scheme is not supported",
            false,
        ),
        SignerErrorKind::UnsupportedOperation => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_operation",
            "operation is not supported",
            false,
        ),
        SignerErrorKind::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "local custody is unavailable",
            true,
        ),
        SignerErrorKind::UserRejected => (
            StatusCode::FORBIDDEN,
            "user_rejected",
            "operation was rejected",
            false,
        ),
        SignerErrorKind::InvalidRequest => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request is invalid",
            false,
        ),
        SignerErrorKind::Other => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "local custody operation failed",
            false,
        ),
    };
    error_response(status, code, message, retryable)
}

fn error_response(status: StatusCode, code: &str, message: &str, retryable: bool) -> Response {
    (
        status,
        Json(wire::ErrorResponse {
            code: code.to_owned(),
            message: message.to_owned(),
            retryable,
        }),
    )
        .into_response()
}

async fn termination_signal() -> AppResult<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(|error| Box::new(error) as AppError),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| Box::new(error) as AppError)
    }
}

async fn shutdown_signal(mut shutdown: watch::Receiver<bool>) {
    if !*shutdown.borrow() {
        drop(shutdown.changed().await);
    }
}

#[derive(Debug)]
struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ConfigError {}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use clap::CommandFactory;

    use super::*;

    fn serve_options(authentication_mode: AuthenticationMode) -> ServeOptions {
        ServeOptions {
            bind: "127.0.0.1:8181".parse().expect("test bind must parse"),
            metrics_bind: "127.0.0.1:9093"
                .parse()
                .expect("test metrics bind must parse"),
            authentication_mode,
            bearer_token: Some("test-token".to_owned()),
            max_request_body_bytes: 1024,
            shutdown_grace_seconds: 1,
        }
    }

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn local_custody_rejects_non_loopback_bind() {
        let options = ServeOptions {
            bind: "0.0.0.0:8181".parse().expect("test bind must parse"),
            ..serve_options(AuthenticationMode::Strict)
        };
        assert!(options.server_config().is_err());
    }

    #[test]
    fn local_custody_rejects_non_loopback_metrics_bind() {
        let options = ServeOptions {
            metrics_bind: "0.0.0.0:9093"
                .parse()
                .expect("test metrics bind must parse"),
            ..serve_options(AuthenticationMode::Strict)
        };
        assert!(options.server_config().is_err());
        assert!(options.metrics_server_config().is_err());
    }

    #[test]
    fn strict_authentication_requires_a_token() {
        let options = ServeOptions {
            bearer_token: None,
            ..serve_options(AuthenticationMode::Strict)
        };
        let error = options
            .server_config()
            .expect_err("strict mode without a token must fail");
        assert!(error.to_string().contains("CUSTODY_BEARER_TOKEN"));
    }

    #[test]
    fn global_trusted_authentication_ignores_an_optional_token() {
        let options = ServeOptions {
            bearer_token: Some("intentionally invalid token".to_owned()),
            ..serve_options(AuthenticationMode::GlobalTrusted)
        };
        options
            .server_config()
            .expect("global-trusted mode must not validate an ignored token");
    }

    #[tokio::test]
    async fn capabilities_report_the_configured_authentication_mode() {
        let state = AppState {
            signer: Arc::new(LocalSigner::ephemeral_for_testing()),
            authentication_mode: AuthenticationMode::GlobalTrusted,
            operations: Arc::new(Mutex::new(BTreeMap::new())),
        };

        let Json(response) = capabilities(State(state)).await;
        assert_eq!(response.authentication_mode, "global_trusted");
    }

    #[tokio::test]
    async fn provision_is_idempotent_and_rejects_changed_content() {
        let state = AppState {
            signer: Arc::new(LocalSigner::ephemeral_for_testing()),
            authentication_mode: AuthenticationMode::Strict,
            operations: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let request = wire::ProvisionRequest {
            operation_id: "provision-1".to_owned(),
            curve: wire::Curve::Secp256k1,
            public_key_format: wire::PublicKeyFormat::Uncompressed,
            purpose: "local-test".to_owned(),
        };

        let first = provision(State(state.clone()), Json(request.clone())).await;
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = to_bytes(first.into_body(), 4096)
            .await
            .expect("first response body must decode");

        let replay = provision(State(state.clone()), Json(request.clone())).await;
        assert_eq!(replay.status(), StatusCode::OK);
        let replay_body = to_bytes(replay.into_body(), 4096)
            .await
            .expect("replay response body must decode");
        assert_eq!(first_body, replay_body);

        let changed = provision(
            State(state),
            Json(wire::ProvisionRequest {
                purpose: "changed-purpose".to_owned(),
                ..request
            }),
        )
        .await;
        assert_eq!(changed.status(), StatusCode::CONFLICT);
    }
}
