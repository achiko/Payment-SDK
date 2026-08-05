//! Authenticated HTTP adapter for a process-separated custody service.
//!
//! This crate depends only on the chain-independent signing contract. It sends
//! opaque key locators, cryptographic payloads, explicit operation IDs, and
//! signature metadata; it never accepts or exports secret key material.

mod client;
mod config;
pub mod wire;

pub use client::{
    CAPABILITIES_PATH, PROVISION_PATH, PUBLIC_KEY_PATH, READINESS_PATH, RemoteSignerClient,
    SIGN_PATH,
};
pub use config::{
    BearerSecret, RemoteRetryPolicy, RemoteSignerConfig, RemoteSignerConfigError,
    RemoteSignerConfigErrorKind, RemoteSignerEndpoint,
};
