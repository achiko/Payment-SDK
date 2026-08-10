//! Version-one JSON wire types for a remote custody service.
//!
//! Enum spellings and field names are explicit so refactors of the Rust
//! contract types cannot silently change the HTTP protocol.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    Secp256k1,
    Ed25519,
    NistP256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicKeyFormat {
    Compressed,
    Uncompressed,
    XOnly,
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureScheme {
    EcdsaSecp256k1,
    SchnorrSecp256k1,
    Ed25519,
    EcdsaNistP256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureEncoding {
    Der,
    Compact,
    Recoverable,
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserInteraction {
    NotRequired,
    Allowed,
    Required,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum KeyLocator {
    Identifier { value: String },
    DerivationPath { children: Vec<ChildIndex> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildIndex {
    pub index: u32,
    pub hardened: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicKey {
    pub curve: Curve,
    pub format: PublicKeyFormat,
    /// Lowercase or uppercase hexadecimal with a required `0x` prefix.
    pub bytes_hex: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionRequest {
    pub operation_id: String,
    pub curve: Curve,
    pub public_key_format: PublicKeyFormat,
    pub purpose: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionResponse {
    pub locator: KeyLocator,
    pub public_key: PublicKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicKeyRequest {
    pub locator: KeyLocator,
    pub curve: Curve,
    pub format: PublicKeyFormat,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicKeyResponse {
    pub public_key: PublicKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SignablePayload {
    Message { bytes_hex: String },
    Digest { bytes_hex: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum KeyTweak {
    Secp256k1Add { scalar_hex: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyTweakKind {
    Secp256k1Add,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignRequest {
    pub operation_id: String,
    pub locator: KeyLocator,
    pub payload: SignablePayload,
    pub scheme: SignatureScheme,
    pub encoding: SignatureEncoding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_tweak: Option<KeyTweak>,
    pub user_interaction: UserInteraction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignResponse {
    pub scheme: SignatureScheme,
    pub encoding: SignatureEncoding,
    pub bytes_hex: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesResponse {
    pub curves: Vec<Curve>,
    pub schemes: Vec<SignatureScheme>,
    #[serde(default)]
    pub key_tweaks: Vec<KeyTweakKind>,
    pub can_sign_messages: bool,
    pub can_sign_digests: bool,
    pub requires_user_interaction: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessStatus {
    Available,
    InteractionRequired,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessResponse {
    pub status: ReadinessStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untweaked_sign_request_omits_the_extension_for_old_servers() {
        let request = SignRequest {
            operation_id: "sign-existing-client".to_owned(),
            locator: KeyLocator::Identifier {
                value: "opaque-key".to_owned(),
            },
            payload: SignablePayload::Digest {
                bytes_hex: format!("0x{}", "11".repeat(32)),
            },
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            key_tweak: None,
            user_interaction: UserInteraction::NotRequired,
        };

        let value = serde_json::to_value(request).expect("wire request must serialize");
        assert!(value.get("key_tweak").is_none());
    }
}
