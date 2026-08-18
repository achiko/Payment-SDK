use serde::{Serialize, de::DeserializeOwned};

use crate::{Error, ErrorKind};

/// An untyped JSON-RPC result retained until a chain adapter decodes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawJson(pub(crate) Vec<u8>);

impl RawJson {
    pub fn new(bytes: Vec<u8>) -> std::result::Result<Self, Error> {
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|_| Error::new(ErrorKind::InvalidRequest, "raw JSON value is invalid"))?;
        Ok(Self(bytes))
    }

    pub fn from_serializable<T>(value: &T) -> std::result::Result<Self, Error>
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

    pub fn deserialize<T>(&self) -> std::result::Result<T, Error>
    where
        T: DeserializeOwned,
    {
        serde_json::from_slice(&self.0).map_err(|_| {
            Error::new(
                ErrorKind::InvalidResponse,
                "JSON-RPC result does not match the requested type",
            )
        })
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: i64,
    pub message: String,
    pub data: Option<RawJson>,
}

impl Failure {
    #[must_use]
    pub const fn is_server_error(&self) -> bool {
        self.code >= -32_099 && self.code <= -32_000
    }
}

pub type CallResult = std::result::Result<RawJson, Failure>;
