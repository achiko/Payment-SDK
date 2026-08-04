//! Future Trezor implementation of the chain-independent `signer::Signer`.
//!
//! Native Trezor transaction protocols that require complete chain
//! transactions are intentionally not hidden inside the generic signer.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrezorSignerConfig {
    pub device_id: Option<String>,
}

#[derive(Debug)]
pub struct TrezorSigner<T> {
    pub transport: T,
    pub config: TrezorSignerConfig,
}
