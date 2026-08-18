//! HTTP adapter for the chain-neutral indexing consumer API.
//!
//! Composition roots may construct [`Remote`] once and expose it as
//! `Arc<dyn indexing::Indexer>`. Business code then depends only on the
//! indexing traits, regardless of whether the implementation is embedded or
//! reached through an Indexer Service.

mod checkpoint;
mod client;
mod config;
mod output;
mod wire;

pub use client::Remote;
pub use config::{Config, ConfigError, ConfigErrorKind};
