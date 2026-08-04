#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignatureScheme {
    EcdsaSecp256k1,
    SchnorrSecp256k1,
    Ed25519,
    EcdsaNistP256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignatureEncoding {
    Der,
    Compact,
    Recoverable,
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub scheme: SignatureScheme,
    pub encoding: SignatureEncoding,
    pub bytes: Vec<u8>,
}
