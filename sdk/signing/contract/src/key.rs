#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyLocator {
    Identifier(String),
    DerivationPath(DerivationPath),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DerivationPath(pub Vec<ChildIndex>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChildIndex {
    pub index: u32,
    pub hardened: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Curve {
    Secp256k1,
    Ed25519,
    NistP256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PublicKeyFormat {
    Compressed,
    Uncompressed,
    XOnly,
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    pub curve: Curve,
    pub format: PublicKeyFormat,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyProvisionRequest {
    pub curve: Curve,
    pub public_key_format: PublicKeyFormat,
    /// Application-supplied purpose or derivation namespace. It contains no chain type.
    pub purpose: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionedKey {
    pub locator: KeyLocator,
    pub public_key: PublicKey,
}
