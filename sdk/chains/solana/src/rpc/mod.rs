//! Singular endpoint-affine Solana RPC ownership.

mod client;
mod config;
mod methods;

#[cfg(test)]
pub(crate) mod test_support;

pub use client::Client;
pub use config::Config;
pub use methods::{Commitment, Context, GenesisHash};
