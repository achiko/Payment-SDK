use std::{fmt, time::Duration};

use json_rpc::Retry;

use crate::Wei;

use super::error::BuildError;

const BASIS_POINTS_DENOMINATOR: u64 = 10_000;

/// Explicit transaction-construction safety limits applied to RPC results.
///
/// Providers remain the source of nonce, gas, and fee observations, but they
/// cannot cause the wallet to build a transaction above operator-selected
/// ceilings. A value over a ceiling is rejected rather than silently clamped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    max_input_bytes: usize,
    gas_limit_margin_basis_points: u32,
    max_gas_limit: u64,
    max_fee_per_gas: Wei,
    max_priority_fee_per_gas: Wei,
    max_total_fee: Wei,
}

impl Limits {
    pub fn new(
        max_input_bytes: usize,
        gas_limit_margin_basis_points: u32,
        max_gas_limit: u64,
        max_fee_per_gas: Wei,
        max_priority_fee_per_gas: Wei,
        max_total_fee: Wei,
    ) -> Result<Self, BuildError> {
        if max_input_bytes == 0 {
            return Err(BuildError::invalid(
                "Ethereum RPC maximum transaction input size must be greater than zero",
            ));
        }
        if u64::from(gas_limit_margin_basis_points) > BASIS_POINTS_DENOMINATOR {
            return Err(BuildError::invalid(
                "Ethereum RPC gas-limit margin must not exceed 10000 basis points",
            ));
        }
        if max_gas_limit == 0 {
            return Err(BuildError::invalid(
                "Ethereum RPC maximum gas limit must be greater than zero",
            ));
        }
        if max_fee_per_gas.is_zero() || max_total_fee.is_zero() {
            return Err(BuildError::invalid(
                "Ethereum RPC fee ceilings must be greater than zero",
            ));
        }
        if max_priority_fee_per_gas > max_fee_per_gas {
            return Err(BuildError::invalid(
                "Ethereum RPC priority-fee ceiling must not exceed the max-fee ceiling",
            ));
        }

        Ok(Self {
            max_input_bytes,
            gas_limit_margin_basis_points,
            max_gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            max_total_fee,
        })
    }

    #[must_use]
    pub const fn max_input_bytes(&self) -> usize {
        self.max_input_bytes
    }

    #[must_use]
    pub const fn gas_limit_margin_basis_points(&self) -> u32 {
        self.gas_limit_margin_basis_points
    }

    #[must_use]
    pub const fn max_gas_limit(&self) -> u64 {
        self.max_gas_limit
    }

    #[must_use]
    pub const fn max_fee_per_gas(&self) -> &Wei {
        &self.max_fee_per_gas
    }

    #[must_use]
    pub const fn max_priority_fee_per_gas(&self) -> &Wei {
        &self.max_priority_fee_per_gas
    }

    #[must_use]
    pub const fn max_total_fee(&self) -> &Wei {
        &self.max_total_fee
    }
}

/// Complete production HTTP configuration for the wallet-facing Ethereum RPC.
///
/// Debug output includes header names for diagnostics, but never the endpoint
/// or header values because both may contain credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpConfig {
    pub(super) endpoints: Vec<String>,
    pub(super) expected_chain_id: u64,
    pub(super) request_timeout: Duration,
    pub(super) max_response_bytes: usize,
    pub(super) headers: Vec<(String, String)>,
    pub(super) retry_policy: Retry,
    pub(super) limits: Limits,
}

impl HttpConfig {
    pub fn new(
        endpoint: impl Into<String>,
        expected_chain_id: u64,
        request_timeout: Duration,
        max_response_bytes: usize,
        retry_policy: Retry,
        limits: Limits,
    ) -> Result<Self, BuildError> {
        let endpoint = endpoint.into();
        if endpoint.trim().is_empty() {
            return Err(BuildError::invalid(
                "Ethereum RPC HTTP endpoint must not be empty",
            ));
        }
        if expected_chain_id == 0 {
            return Err(BuildError::invalid(
                "expected Ethereum chain ID must be non-zero",
            ));
        }
        if request_timeout.is_zero() {
            return Err(BuildError::invalid(
                "Ethereum RPC request timeout must be greater than zero",
            ));
        }
        if max_response_bytes == 0 {
            return Err(BuildError::invalid(
                "Ethereum RPC response-size limit must be greater than zero",
            ));
        }

        Ok(Self {
            endpoints: vec![endpoint],
            expected_chain_id,
            request_timeout,
            max_response_bytes,
            headers: Vec::new(),
            retry_policy,
            limits,
        })
    }

    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Replaces the endpoint list. Calls try endpoints in this exact order.
    pub fn with_endpoints<I, S>(mut self, endpoints: I) -> Result<Self, BuildError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let endpoints = endpoints.into_iter().map(Into::into).collect::<Vec<_>>();
        if endpoints.is_empty() || endpoints.iter().any(|endpoint| endpoint.trim().is_empty()) {
            return Err(BuildError::invalid(
                "Ethereum RPC endpoints must contain at least one non-empty endpoint",
            ));
        }
        self.endpoints = endpoints;
        Ok(self)
    }

    #[must_use]
    pub const fn expected_chain_id(&self) -> u64 {
        self.expected_chain_id
    }

    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }
}

impl fmt::Debug for HttpConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        formatter
            .debug_struct("HttpConfig")
            .field("endpoints", &vec!["[REDACTED]"; self.endpoints.len()])
            .field("expected_chain_id", &self.expected_chain_id)
            .field("request_timeout", &self.request_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("header_names", &header_names)
            .field("retry_policy", &self.retry_policy)
            .field("limits", &self.limits)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use json_rpc::Retry;

    use super::*;
    use crate::Wei;

    fn limits() -> Limits {
        Limits::new(
            1024,
            2_000,
            1_000_000,
            Wei::from_u128(1_000_000_000_000),
            Wei::from_u128(100_000_000_000),
            Wei::from_u128(1_000_000_000_000_000_000),
        )
        .expect("test limits must be valid")
    }

    #[test]
    fn multiple_endpoints_are_ordered_and_redacted() {
        let config = HttpConfig::new(
            "https://first.invalid/rpc?key=first-secret",
            31_337,
            Duration::from_secs(1),
            1024,
            Retry::no_retry(),
            limits(),
        )
        .expect("base endpoint must be valid")
        .with_endpoints([
            "https://first.invalid/rpc?key=first-secret",
            "https://second.invalid/rpc?key=second-secret",
        ])
        .expect("ordered endpoints must be valid");

        assert_eq!(
            config.endpoints,
            [
                "https://first.invalid/rpc?key=first-secret",
                "https://second.invalid/rpc?key=second-secret",
            ]
        );
        let debug = format!("{config:?}");
        assert_eq!(debug.matches("[REDACTED]").count(), 2);
        assert!(!debug.contains("first-secret"));
        assert!(!debug.contains("second-secret"));

        let error = config
            .with_endpoints(Vec::<String>::new())
            .expect_err("an empty endpoint set must fail");
        assert_eq!(
            error.kind,
            super::super::BuildErrorKind::InvalidConfiguration
        );
    }
}
