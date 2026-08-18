//! Transport boundary used by the Bitcoin Core method wrapper.
//!
//! `corepc-client` is deliberately not used: its upstream project marks it as
//! unsuitable for production. The chain wrapper therefore retains async
//! execution, authentication, retry, timeout, and response-size policy in the
//! repository's transport packages while using `rust-bitcoin` for protocol
//! parsing and consensus types.

pub(crate) use json_rpc::{Client, Error, Failure, RawJson, Request, RequestId};
