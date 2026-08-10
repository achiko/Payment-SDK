use crate::{BearerSecret, RemoteRetryPolicy, RemoteSignerConfig, RemoteSignerEndpoint, wire};
use reqwest::{Method, StatusCode, redirect::Policy};
use serde::{Serialize, de::DeserializeOwned};
use signer::{
    BoxFuture, ChildIndex, Curve, DerivationPath, Digest, KeyLocator, KeyProvisionRequest,
    KeyProvisioner, KeyTweak, KeyTweakKind, OperationId, ProvisionedKey, PublicKey,
    PublicKeyFormat, SignRequest, SignablePayload, Signature, SignatureEncoding, SignatureScheme,
    Signer, SignerCapabilities, SignerError, SignerErrorKind, SignerStatus, UserInteraction,
};
use std::{fmt, time::Duration};

pub const PROVISION_PATH: &str = "v1/keys/provision";
pub const PUBLIC_KEY_PATH: &str = "v1/keys/public-key";
pub const SIGN_PATH: &str = "v1/signatures";
pub const READINESS_PATH: &str = "v1/readiness";
pub const CAPABILITIES_PATH: &str = "v1/capabilities";

const MAX_OPERATION_ID_BYTES: usize = 256;

/// Authenticated, chain-independent client for a process-separated custody service.
///
/// Construction fetches and caches capabilities because [`Signer::capabilities`]
/// is synchronous. Readiness remains a live request through [`Signer::status`].
#[derive(Clone)]
pub struct RemoteSignerClient {
    endpoint: RemoteSignerEndpoint,
    bearer_secret: BearerSecret,
    request_timeout: Duration,
    max_response_bytes: usize,
    retry_policy: RemoteRetryPolicy,
    client: reqwest::Client,
    capabilities: SignerCapabilities,
}

impl RemoteSignerClient {
    /// Builds a no-redirect HTTP client and loads the remote capability snapshot.
    ///
    /// Capability discovery is attempted once. Only provision/sign calls that
    /// carry an operation ID use the configured retry policy.
    pub async fn connect(config: RemoteSignerConfig) -> Result<Self, SignerError> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .build()
            .map_err(|_| unavailable("failed to construct remote custody HTTP client"))?;
        let mut result = Self {
            endpoint: config.endpoint,
            bearer_secret: config.bearer_secret,
            request_timeout: config.request_timeout,
            max_response_bytes: config.max_response_bytes,
            retry_policy: config.retry_policy,
            client,
            capabilities: SignerCapabilities {
                curves: Vec::new(),
                schemes: Vec::new(),
                key_tweaks: Vec::new(),
                can_sign_messages: false,
                can_sign_digests: false,
                requires_user_interaction: false,
            },
        };
        result.capabilities = result.fetch_capabilities().await?;
        Ok(result)
    }

    pub async fn provision_key(
        &self,
        request: KeyProvisionRequest,
    ) -> Result<ProvisionedKey, SignerError> {
        validate_operation_id(&request.operation_id)?;
        if request.purpose.trim().is_empty() {
            return Err(invalid_request("key purpose must not be empty"));
        }
        let expected_curve = request.curve;
        let expected_format = request.public_key_format;
        let body = wire::ProvisionRequest {
            operation_id: request.operation_id.as_str().to_owned(),
            curve: curve_to_wire(request.curve),
            public_key_format: public_key_format_to_wire(request.public_key_format),
            purpose: request.purpose,
        };
        let response: wire::ProvisionResponse = self
            .send_json(Method::POST, PROVISION_PATH, &body, RetryMode::Operation)
            .await?;
        let public_key = public_key_from_wire(response.public_key)?;
        if public_key.curve != expected_curve || public_key.format != expected_format {
            return Err(protocol_error(
                "remote custody returned a provisioned public key with mismatched metadata",
            ));
        }
        Ok(ProvisionedKey {
            locator: locator_from_wire(response.locator)?,
            public_key,
        })
    }

    pub async fn lookup_public_key(
        &self,
        key: &KeyLocator,
        curve: Curve,
        format: PublicKeyFormat,
    ) -> Result<PublicKey, SignerError> {
        let body = wire::PublicKeyRequest {
            locator: locator_to_wire(key)?,
            curve: curve_to_wire(curve),
            format: public_key_format_to_wire(format),
        };
        let response: wire::PublicKeyResponse = self
            .send_json(Method::POST, PUBLIC_KEY_PATH, &body, RetryMode::Never)
            .await?;
        let public_key = public_key_from_wire(response.public_key)?;
        if public_key.curve != curve || public_key.format != format {
            return Err(protocol_error(
                "remote custody returned a public key with mismatched metadata",
            ));
        }
        Ok(public_key)
    }

    pub async fn sign_payload(&self, request: SignRequest) -> Result<Signature, SignerError> {
        validate_operation_id(&request.operation_id)?;
        let expected_scheme = request.scheme;
        let expected_encoding = request.encoding;
        let body = wire::SignRequest {
            operation_id: request.operation_id.as_str().to_owned(),
            locator: locator_to_wire(&request.key)?,
            payload: payload_to_wire(request.payload),
            scheme: signature_scheme_to_wire(request.scheme),
            encoding: signature_encoding_to_wire(request.encoding),
            key_tweak: request.key_tweak.map(key_tweak_to_wire),
            user_interaction: user_interaction_to_wire(request.user_interaction),
        };
        let response: wire::SignResponse = self
            .send_json(Method::POST, SIGN_PATH, &body, RetryMode::Operation)
            .await?;
        let signature = signature_from_wire(response)?;
        if signature.scheme != expected_scheme || signature.encoding != expected_encoding {
            return Err(protocol_error(
                "remote custody returned a signature with mismatched metadata",
            ));
        }
        Ok(signature)
    }

    pub async fn readiness(&self) -> Result<SignerStatus, SignerError> {
        let response: wire::ReadinessResponse =
            self.send_without_body(Method::GET, READINESS_PATH).await?;
        Ok(match response.status {
            wire::ReadinessStatus::Available => SignerStatus::Available,
            wire::ReadinessStatus::InteractionRequired => SignerStatus::InteractionRequired,
            wire::ReadinessStatus::Unavailable => SignerStatus::Unavailable {
                reason: "remote custody reports unavailable".to_owned(),
            },
        })
    }

    pub async fn fetch_capabilities(&self) -> Result<SignerCapabilities, SignerError> {
        let response: wire::CapabilitiesResponse = self
            .send_without_body(Method::GET, CAPABILITIES_PATH)
            .await?;
        Ok(SignerCapabilities {
            curves: response.curves.into_iter().map(curve_from_wire).collect(),
            schemes: response
                .schemes
                .into_iter()
                .map(signature_scheme_from_wire)
                .collect(),
            key_tweaks: response
                .key_tweaks
                .into_iter()
                .map(key_tweak_kind_from_wire)
                .collect(),
            can_sign_messages: response.can_sign_messages,
            can_sign_digests: response.can_sign_digests,
            requires_user_interaction: response.requires_user_interaction,
        })
    }

    async fn send_without_body<T>(&self, method: Method, path: &str) -> Result<T, SignerError>
    where
        T: DeserializeOwned,
    {
        self.send_bytes(method, path, None, RetryMode::Never).await
    }

    async fn send_json<T, B>(
        &self,
        method: Method,
        path: &str,
        body: &B,
        retry_mode: RetryMode,
    ) -> Result<T, SignerError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let body = serde_json::to_vec(body)
            .map_err(|_| invalid_request("failed to encode remote custody request"))?;
        self.send_bytes(method, path, Some(&body), retry_mode).await
    }

    async fn send_bytes<T>(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        retry_mode: RetryMode,
    ) -> Result<T, SignerError>
    where
        T: DeserializeOwned,
    {
        let url = self
            .endpoint
            .route(path)
            .map_err(|_| invalid_request("remote custody route is invalid"))?;
        let mut attempt = 1_u32;
        loop {
            match self.send_once(method.clone(), url.clone(), body).await {
                Ok(value) => return Ok(value),
                Err(error)
                    if retry_mode == RetryMode::Operation
                        && error.retryable
                        && attempt < self.retry_policy.max_attempts.get() =>
                {
                    tokio::time::sleep(self.retry_policy.backoff_after(attempt)).await;
                    attempt += 1;
                }
                Err(error) => return Err(error.error),
            }
        }
    }

    async fn send_once<T>(
        &self,
        method: Method,
        url: reqwest::Url,
        body: Option<&[u8]>,
    ) -> Result<T, AttemptError>
    where
        T: DeserializeOwned,
    {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(self.bearer_secret.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(self.request_timeout);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_vec());
        }
        let response = request.send().await.map_err(map_reqwest_error)?;
        let status = response.status();
        let body = bounded_body(response, self.max_response_bytes).await?;
        if !status.is_success() {
            return Err(remote_error(status, &body));
        }
        serde_json::from_slice(&body).map_err(|_| AttemptError {
            error: protocol_error("remote custody returned an invalid JSON response"),
            retryable: false,
        })
    }
}

const fn key_tweak_kind_from_wire(value: wire::KeyTweakKind) -> KeyTweakKind {
    match value {
        wire::KeyTweakKind::Secp256k1Add => KeyTweakKind::Secp256k1Add,
    }
}

impl fmt::Debug for RemoteSignerClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteSignerClient")
            .field("endpoint", &self.endpoint)
            .field("bearer_secret", &self.bearer_secret)
            .field("request_timeout", &self.request_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("retry_policy", &self.retry_policy)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl KeyProvisioner for RemoteSignerClient {
    fn provision<'a>(
        &'a self,
        request: KeyProvisionRequest,
    ) -> BoxFuture<'a, Result<ProvisionedKey, SignerError>> {
        Box::pin(async move { self.provision_key(request).await })
    }
}

impl Signer for RemoteSignerClient {
    fn capabilities(&self) -> SignerCapabilities {
        self.capabilities.clone()
    }

    fn status<'a>(&'a self) -> BoxFuture<'a, Result<SignerStatus, SignerError>> {
        Box::pin(async move { self.readiness().await })
    }

    fn public_key<'a>(
        &'a self,
        key: &'a KeyLocator,
        curve: Curve,
        format: PublicKeyFormat,
    ) -> BoxFuture<'a, Result<PublicKey, SignerError>> {
        Box::pin(async move { self.lookup_public_key(key, curve, format).await })
    }

    fn sign<'a>(&'a self, request: SignRequest) -> BoxFuture<'a, Result<Signature, SignerError>> {
        Box::pin(async move { self.sign_payload(request).await })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryMode {
    Never,
    Operation,
}

struct AttemptError {
    error: SignerError,
    retryable: bool,
}

async fn bounded_body(
    mut response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, AttemptError> {
    if response
        .content_length()
        .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > maximum))
    {
        return Err(AttemptError {
            error: protocol_error("remote custody response exceeds the configured size limit"),
            retryable: false,
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
        let next_length = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| AttemptError {
                error: protocol_error("remote custody response size overflowed"),
                retryable: false,
            })?;
        if next_length > maximum {
            return Err(AttemptError {
                error: protocol_error("remote custody response exceeds the configured size limit"),
                retryable: false,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn map_reqwest_error(error: reqwest::Error) -> AttemptError {
    let (message, retryable) = if error.is_timeout() {
        ("remote custody request timed out", true)
    } else if error.is_connect() || error.is_request() {
        ("remote custody endpoint is unavailable", true)
    } else {
        ("remote custody HTTP request failed", false)
    };
    AttemptError {
        error: unavailable(message),
        retryable,
    }
}

fn remote_error(status: StatusCode, body: &[u8]) -> AttemptError {
    let decoded = serde_json::from_slice::<wire::ErrorResponse>(body).ok();
    let code = decoded.as_ref().map(|error| error.code.as_str());
    let retryable = retryable_status(status)
        || decoded
            .as_ref()
            .is_some_and(|error| error.retryable && status.is_server_error());
    let error = match code {
        Some("operation_changed") => {
            invalid_request("remote custody operation ID was reused with different request content")
        }
        Some("key_not_found") => signer_error(
            SignerErrorKind::KeyNotFound,
            "remote custody key was not found",
        ),
        Some("unsupported_curve") => signer_error(
            SignerErrorKind::UnsupportedCurve,
            "remote custody does not support the requested curve",
        ),
        Some("unsupported_scheme") => signer_error(
            SignerErrorKind::UnsupportedScheme,
            "remote custody does not support the requested signature scheme",
        ),
        Some("unsupported_operation") => signer_error(
            SignerErrorKind::UnsupportedOperation,
            "remote custody does not support the requested operation",
        ),
        Some("user_rejected") => signer_error(
            SignerErrorKind::UserRejected,
            "remote custody operation was rejected by the user",
        ),
        Some("invalid_request" | "invalid_operation_id") => {
            invalid_request("remote custody rejected the request")
        }
        Some("unavailable") => unavailable("remote custody is unavailable"),
        _ if status == StatusCode::CONFLICT => {
            invalid_request("remote custody operation ID was reused with different request content")
        }
        _ if status == StatusCode::NOT_FOUND => signer_error(
            SignerErrorKind::KeyNotFound,
            "remote custody resource was not found",
        ),
        _ if status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY => {
            invalid_request("remote custody rejected the request")
        }
        _ if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN => {
            unavailable("remote custody authentication failed")
        }
        _ if retryable_status(status) => unavailable("remote custody is temporarily unavailable"),
        _ => signer_error(
            SignerErrorKind::Other,
            format!(
                "remote custody request failed with HTTP status {}",
                status.as_u16()
            ),
        ),
    };
    AttemptError { error, retryable }
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn validate_operation_id(operation_id: &OperationId) -> Result<(), SignerError> {
    if operation_id.as_str().trim().is_empty() {
        return Err(invalid_request(
            "remote custody operation ID must not be empty",
        ));
    }
    if operation_id.as_str().len() > MAX_OPERATION_ID_BYTES {
        return Err(invalid_request(
            "remote custody operation ID exceeds 256 bytes",
        ));
    }
    Ok(())
}

fn locator_to_wire(locator: &KeyLocator) -> Result<wire::KeyLocator, SignerError> {
    match locator {
        KeyLocator::Identifier(value) if value.trim().is_empty() => {
            Err(invalid_request("key locator identifier must not be empty"))
        }
        KeyLocator::Identifier(value) => Ok(wire::KeyLocator::Identifier {
            value: value.clone(),
        }),
        KeyLocator::DerivationPath(path) => Ok(wire::KeyLocator::DerivationPath {
            children: path
                .0
                .iter()
                .map(|child| wire::ChildIndex {
                    index: child.index,
                    hardened: child.hardened,
                })
                .collect(),
        }),
    }
}

fn locator_from_wire(locator: wire::KeyLocator) -> Result<KeyLocator, SignerError> {
    match locator {
        wire::KeyLocator::Identifier { value } if value.trim().is_empty() => Err(protocol_error(
            "remote custody returned an empty key locator",
        )),
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

fn payload_to_wire(payload: SignablePayload) -> wire::SignablePayload {
    match payload {
        SignablePayload::Message(bytes) => wire::SignablePayload::Message {
            bytes_hex: encode_hex(&bytes),
        },
        SignablePayload::Digest(Digest { bytes }) => wire::SignablePayload::Digest {
            bytes_hex: encode_hex(&bytes),
        },
    }
}

fn key_tweak_to_wire(tweak: KeyTweak) -> wire::KeyTweak {
    match tweak {
        KeyTweak::Secp256k1Add(scalar) => wire::KeyTweak::Secp256k1Add {
            scalar_hex: encode_hex(&scalar),
        },
    }
}

fn public_key_from_wire(public_key: wire::PublicKey) -> Result<PublicKey, SignerError> {
    let bytes = decode_non_empty_hex(&public_key.bytes_hex, "public key")?;
    Ok(PublicKey {
        curve: curve_from_wire(public_key.curve),
        format: public_key_format_from_wire(public_key.format),
        bytes,
    })
}

fn signature_from_wire(response: wire::SignResponse) -> Result<Signature, SignerError> {
    Ok(Signature {
        scheme: signature_scheme_from_wire(response.scheme),
        encoding: signature_encoding_from_wire(response.encoding),
        bytes: decode_non_empty_hex(&response.bytes_hex, "signature")?,
    })
}

fn encode_hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn decode_non_empty_hex(value: &str, field: &str) -> Result<Vec<u8>, SignerError> {
    let encoded = value
        .strip_prefix("0x")
        .ok_or_else(|| protocol_error(format!("remote custody {field} is not 0x-prefixed hex")))?;
    if encoded.is_empty() {
        return Err(protocol_error(format!(
            "remote custody returned an empty {field}"
        )));
    }
    hex::decode(encoded)
        .map_err(|_| protocol_error(format!("remote custody returned invalid {field} hex")))
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

fn user_interaction_to_wire(value: UserInteraction) -> wire::UserInteraction {
    match value {
        UserInteraction::NotRequired => wire::UserInteraction::NotRequired,
        UserInteraction::Allowed => wire::UserInteraction::Allowed,
        UserInteraction::Required => wire::UserInteraction::Required,
    }
}

fn signer_error(kind: SignerErrorKind, message: impl Into<String>) -> SignerError {
    SignerError {
        kind,
        message: message.into(),
    }
}

fn unavailable(message: impl Into<String>) -> SignerError {
    signer_error(SignerErrorKind::Unavailable, message)
}

fn invalid_request(message: impl Into<String>) -> SignerError {
    signer_error(SignerErrorKind::InvalidRequest, message)
}

fn protocol_error(message: impl Into<String>) -> SignerError {
    signer_error(SignerErrorKind::Other, message)
}
