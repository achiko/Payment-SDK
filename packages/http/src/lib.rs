//! Configuration surface for a future HTTP implementation of `transport`.

use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpTransportConfig {
    pub endpoint: String,
    pub request_timeout: Duration,
    pub default_headers: Vec<(String, String)>,
}

/// Concrete Hyper/HTTP behavior is intentionally not implemented yet.
#[derive(Clone, Debug)]
pub struct HttpTransport {
    pub config: HttpTransportConfig,
}
