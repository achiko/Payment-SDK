//! Durable record of which addresses an indexer must observe.
//!
//! Synchronization takes a complete filter snapshot on every call and keeps no
//! state of its own. This is where that snapshot survives a restart: the
//! registry stores the selection, and a caller reloads it before the first
//! sync so the indexer resumes with the address set it had.

use std::fmt;

use crate::{AddressFilter, BoxFuture, IndexError, IndexScope};

/// One observed address and the caller's opaque material for it.
#[derive(Clone, PartialEq, Eq)]
pub struct RegisteredAddress {
    /// Caller-owned identity, unique across the registry.
    pub id: String,
    /// The canonical address and the first block worth inspecting.
    pub filter: AddressFilter,
    /// Bytes stored verbatim on the caller's behalf and handed back unchanged.
    ///
    /// Indexing never interprets, validates, or logs them. An application that
    /// keeps custody material here is choosing where its keys live; indexing
    /// only guarantees the bytes come back as they went in.
    pub material: Vec<u8>,
}

/// Redacts `material`: it routinely holds key material, and a stray debug line
/// is exactly how such bytes escape into logs.
impl fmt::Debug for RegisteredAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegisteredAddress")
            .field("id", &self.id)
            .field("filter", &self.filter)
            .field(
                "material",
                &format_args!("<{} bytes redacted>", self.material.len()),
            )
            .finish()
    }
}

/// Persistent address selection for one scope.
pub trait Registry: Send + Sync {
    /// Records one address. Fails with `Conflict` if the identity or the
    /// address is already registered, rather than quietly replacing it.
    fn register<'a>(&'a self, entry: RegisteredAddress) -> BoxFuture<'a, Result<(), IndexError>>;

    /// Every address registered for `scope`, for rebuilding a filter snapshot.
    fn registered<'a>(
        &'a self,
        scope: &'a IndexScope,
    ) -> BoxFuture<'a, Result<Vec<RegisteredAddress>, IndexError>>;
}
