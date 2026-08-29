use std::{fmt, time::Duration};

use crate::{Error, ErrorKind};

/// One endpoint-affine Solana RPC configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    endpoint: String,
    headers: Vec<(String, String)>,
    timeout: Duration,
    max_request_bytes: usize,
    max_response_bytes: usize,
}

impl Config {
    pub fn new(
        endpoint: impl Into<String>,
        timeout: Duration,
        max_request_bytes: usize,
        max_response_bytes: usize,
    ) -> Result<Self, Error> {
        let endpoint = endpoint.into();
        if endpoint.trim().is_empty()
            || timeout.is_zero()
            || max_request_bytes == 0
            || max_response_bytes == 0
        {
            return Err(Error::new(
                ErrorKind::InvalidRpcConfiguration,
                "Solana RPC endpoint, timeout, and size limits must be configured",
            ));
        }
        Ok(Self {
            endpoint,
            headers: Vec::new(),
            timeout,
            max_request_bytes,
            max_response_bytes,
        })
    }

    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub(super) fn into_transport(self) -> json_rpc::Config {
        let mut config = json_rpc::Config::new(self.endpoint, self.timeout);
        config.max_request_bytes = self.max_request_bytes;
        config.max_response_bytes = self.max_response_bytes;
        config.headers = self.headers;
        config.retry = json_rpc::Retry::no_retry();
        config
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("endpoint_count", &1)
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("timeout", &self.timeout)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn represents_one_redacted_bounded_endpoint() {
        let config = Config::new(
            "https://user:secret@example.invalid",
            Duration::from_secs(2),
            1_024,
            2_048,
        )
        .expect("valid config")
        .with_header("authorization", "Bearer hidden");
        let debug = format!("{config:?}");
        assert!(debug.contains("endpoint_count: 1"));
        assert!(debug.contains("authorization"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("Bearer hidden"));
        let transport = config.into_transport();
        assert_eq!(transport.endpoints.len(), 1);
        assert_eq!(transport.retry, json_rpc::Retry::no_retry());
    }

    #[test]
    fn rejects_every_empty_bound() {
        for result in [
            Config::new("", Duration::from_secs(1), 1, 1),
            Config::new("endpoint", Duration::ZERO, 1, 1),
            Config::new("endpoint", Duration::from_secs(1), 0, 1),
            Config::new("endpoint", Duration::from_secs(1), 1, 0),
        ] {
            assert_eq!(
                result.unwrap_err().kind(),
                ErrorKind::InvalidRpcConfiguration
            );
        }
    }
}
