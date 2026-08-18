#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SignatureScheme {
    EcdsaSecp256k1,
    SchnorrSecp256k1,
    Ed25519,
    EcdsaNistP256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SignatureEncoding {
    Der,
    Compact,
    Recoverable,
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Signature {
    pub scheme: SignatureScheme,
    pub encoding: SignatureEncoding,
    pub bytes: Vec<u8>,
}
