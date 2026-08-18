//! Transport boundary used by the Ethereum method wrapper.
//!
//! The wrapper remains generic over request execution so applications can use
//! the production HTTP client or deterministic test doubles. Alloy's provider
//! crate can replace this boundary when a release compatible with the
//! workspace MSRV and pinned Alloy types is selected.

pub(crate) use json_rpc::{
    Client, Error, Failover, Failure, RawJson, Request, RequestId, Response, TransportClient,
};
