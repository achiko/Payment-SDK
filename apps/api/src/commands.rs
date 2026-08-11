use axum::{
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use deposits::{IdempotencyKey, RequestHash};
use http_support::AuthenticationMode;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::api_error::ApiError;

const IDEMPOTENCY_KEY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
const MAX_OPAQUE_ID_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveIdempotencyKey {
    pub key: IdempotencyKey,
    pub generated: bool,
}

pub fn idempotency_key(
    headers: &HeaderMap,
    authentication_mode: AuthenticationMode,
) -> Result<EffectiveIdempotencyKey, ApiError> {
    let Some(value) = headers.get(&IDEMPOTENCY_KEY_HEADER) else {
        return if authentication_mode.is_strict() {
            Err(ApiError::bad_request(
                "missing_idempotency_key",
                "Idempotency-Key header is required",
            ))
        } else {
            Ok(EffectiveIdempotencyKey {
                key: IdempotencyKey(Uuid::now_v7().to_string()),
                generated: true,
            })
        };
    };
    let value = value.to_str().map_err(|_| {
        ApiError::bad_request(
            "invalid_idempotency_key",
            "Idempotency-Key must contain visible ASCII characters",
        )
    })?;
    validate_token(value, MAX_IDEMPOTENCY_KEY_BYTES, "Idempotency-Key")?;
    Ok(EffectiveIdempotencyKey {
        key: IdempotencyKey(value.to_owned()),
        generated: false,
    })
}

pub fn idempotent_response(
    response: impl IntoResponse,
    generated_idempotency_key: Option<&IdempotencyKey>,
) -> Result<Response, ApiError> {
    let mut response = response.into_response();
    let Some(idempotency_key) = generated_idempotency_key else {
        return Ok(response);
    };
    let value = HeaderValue::from_str(&idempotency_key.0).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_stored_idempotency_key",
            "stored idempotency identity cannot be represented as an HTTP header",
            false,
        )
    })?;
    response.headers_mut().insert(IDEMPOTENCY_KEY_HEADER, value);
    Ok(response)
}

pub fn validate_opaque_id(value: &str, name: &str) -> Result<(), ApiError> {
    validate_token(value, MAX_OPAQUE_ID_BYTES, name)
}

fn validate_token(value: &str, maximum: usize, name: &str) -> Result<(), ApiError> {
    if value.is_empty() || value.len() > maximum {
        return Err(ApiError::bad_request(
            "invalid_identifier",
            format!("{name} must contain between 1 and {maximum} bytes"),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\\'))
    {
        return Err(ApiError::bad_request(
            "invalid_identifier",
            format!("{name} must use visible ASCII without quotes or backslashes"),
        ));
    }
    Ok(())
}

/// Hashes normalized semantic fields with length delimiters. JSON whitespace,
/// object ordering, and transport framing therefore do not change command
/// identity, while field-boundary ambiguity is impossible.
#[must_use]
pub fn request_hash(domain: &str, fields: &[&str]) -> RequestHash {
    let mut hasher = Sha256::new();
    update_field(&mut hasher, b"payment-service-command-v1");
    update_field(&mut hasher, domain.as_bytes());
    for field in fields {
        update_field(&mut hasher, field.as_bytes());
    }
    RequestHash(hasher.finalize().into())
}

fn update_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(field);
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn idempotency_header_is_required_and_bounded() {
        assert!(idempotency_key(&HeaderMap::new(), AuthenticationMode::Strict).is_err());

        let generated = idempotency_key(&HeaderMap::new(), AuthenticationMode::GlobalTrusted)
            .expect("global-trusted mode must generate a key");
        assert!(generated.generated);
        assert!(Uuid::parse_str(&generated.key.0).is_ok());

        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("exchange-command-1"),
        );
        assert_eq!(
            idempotency_key(&headers, AuthenticationMode::Strict).expect("valid key must parse"),
            EffectiveIdempotencyKey {
                key: IdempotencyKey("exchange-command-1".to_owned()),
                generated: false,
            }
        );

        let oversized = "a".repeat(MAX_IDEMPOTENCY_KEY_BYTES + 1);
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_str(&oversized).expect("visible ASCII must form a header"),
        );
        assert!(idempotency_key(&headers, AuthenticationMode::GlobalTrusted).is_err());
    }

    #[test]
    fn only_generated_idempotency_key_is_returned_as_a_response_header() {
        let key = IdempotencyKey("caller-command-7".to_owned());
        let response = idempotent_response(StatusCode::ACCEPTED, Some(&key))
            .expect("validated idempotency key must form a response header");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response
                .headers()
                .get(IDEMPOTENCY_KEY_HEADER)
                .expect("response header must exist"),
            "caller-command-7"
        );

        let response = idempotent_response(StatusCode::ACCEPTED, None)
            .expect("caller-supplied identities do not alter the response");
        assert!(response.headers().get(IDEMPOTENCY_KEY_HEADER).is_none());
    }

    #[test]
    fn opaque_identifiers_reject_ambiguous_or_invisible_characters() {
        assert!(validate_opaque_id("user-42", "user_id").is_ok());
        assert!(validate_opaque_id("", "user_id").is_err());
        assert!(validate_opaque_id("user 42", "user_id").is_err());
        assert!(validate_opaque_id("user\\42", "user_id").is_err());
    }

    #[test]
    fn semantic_hash_is_stable_and_field_delimited() {
        let first = request_hash("create_deposit", &["ab", "c"]);
        let replay = request_hash("create_deposit", &["ab", "c"]);
        let different_boundaries = request_hash("create_deposit", &["a", "bc"]);
        let different_operation = request_hash("close_deposit", &["ab", "c"]);

        assert_eq!(first, replay);
        assert_ne!(first, different_boundaries);
        assert_ne!(first, different_operation);
    }
}
