//! Transport boundary used by the Ethereum method wrapper.
//!
//! The wrapper remains generic over request execution so applications can use
//! the production `jsonrpsee` client or deterministic test doubles.

pub(crate) use json_rpc::{Call, Client, Error, Failure, RawJson};
